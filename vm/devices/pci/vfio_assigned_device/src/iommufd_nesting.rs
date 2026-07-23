// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! iommufd nested translation for VFIO devices behind an accel-capable SMMU.
//!
//! This module implements HW-accelerated nested stage 1 translation using
//! iommufd. The guest programs the emulated SMMU's stream table entries (STEs)
//! and page tables. The SMMU emulator decodes the guest's CMDQ commands and
//! dispatches a [`smmu::StreamConfig`] to this module via the per-device
//! [`smmu::AcceleratedStreamBackend`] trait, and forwards batched invalidation
//! commands via the per-vIOMMU [`smmu::AcceleratedInvalidationSink`] trait,
//! both of which program the host IOMMU hardware.
//!
//! # Architecture
//!
//! ```text
//! Guest programs emulated SMMU ──► CMDQ commands
//!        │
//!        ▼
//! SmmuDevice decodes STE/CMDQ and dispatches:
//!   ├─ STE config ──► IommufdStreamBackend (per VFIO device)
//!   │     └─ set_stream_config: map StreamConfig → allocate/switch nested HWPT
//!   └─ invalidation batch ──► SmmuAccelState (per vIOMMU)
//!         └─ invalidate: forward ordered batch to iommufd HWPT_INVALIDATE
//!        │
//!        ▼
//! Host IOMMU HW walks guest S1 tables ──► physical DMA
//! ```
//!
//! # Object Lifecycle
//!
//! - [`SmmuAccelState`]: per-SMMU iommufd objects (vIOMMU). Created lazily on
//!   first VFIO device attachment. Shared across all devices behind the same
//!   SMMU. Implements [`smmu::AcceleratedInvalidationSink`]: invalidation is
//!   vIOMMU-scoped, so one batched `IOMMU_HWPT_INVALIDATE` per guest command
//!   covers every stream behind the SMMU.
//! - [`IommufdStreamBackend`]: per-device stream backend. Created during VFIO
//!   cdev device resolution. Registered with [`smmu::SmmuSharedState`] by
//!   stream ID.

use anyhow::Context as _;
use parking_lot::Mutex;
use std::sync::Arc;
use vfio_sys::iommufd::IommufdCtx;

/// Query the physical SMMUv3's capabilities for a device bound to iommufd.
///
/// Issues a single `IOMMU_GET_HW_INFO` and hands the host's raw IDR registers
/// to [`smmu::HostSmmuCaps::from_idr`], which decodes the fields the vSMMU
/// finalizes against and validates compatibility with (OAS, TTF, TTENDIAN,
/// GRAN4K).
pub fn query_host_caps(ctx: &IommufdCtx, dev_id: u32) -> anyhow::Result<smmu::HostSmmuCaps> {
    let mut info = vfio_sys::iommufd::IommuHwInfoArmSmmuv3 {
        flags: 0,
        __reserved: 0,
        idr: [0; 6],
        iidr: 0,
        aidr: 0,
    };
    let (data_type, _caps) = ctx
        .get_hw_info(dev_id, &mut info)
        .context("IOMMU_GET_HW_INFO failed")?;
    if data_type != vfio_sys::iommufd::IOMMU_HW_INFO_TYPE_ARM_SMMUV3 {
        anyhow::bail!("unexpected host IOMMU hw info type {data_type} (expected ARM SMMUv3)");
    }
    Ok(smmu::HostSmmuCaps::from_idr(info.idr))
}

/// Nested STE double-words `[DW0, DW1]` for the persistent **abort** HWPT:
/// `STE.V=1` (bit 0), `STE.Config=0b000` (abort). All other fields RES0.
const ABORT_STE_DWORDS: [u64; 2] = [0b1, 0];
/// Nested STE double-words `[DW0, DW1]` for the persistent **bypass** HWPT:
/// `STE.V=1` (bit 0), `STE.Config=0b100` (S1 bypass over the S2 parent; bit 3).
/// All other fields RES0.
const BYPASS_STE_DWORDS: [u64; 2] = [0b1001, 0];

/// Per-SMMU iommufd objects for HW-accelerated nested translation.
///
/// Created lazily on first VFIO device attachment for an accel-capable SMMU.
/// Shared (via `Arc`) across all [`IommufdStreamBackend`] instances behind
/// the same SMMU.
///
/// The vIOMMU represents the emulated SMMU in the iommufd object model.
/// Per-device S1 translation HWPTs and vDevices are allocated under it, as are
/// the two shared, persistent nested HWPTs (abort and bypass) that every device
/// attaches to in those states — so a device is always a member of a nested
/// HWPT under this vIOMMU, never detached to the raw blocking/S2 domains.
pub struct SmmuAccelState {
    /// The iommufd context (shared with IoasManager).
    ctx: Arc<IommufdCtx>,
    /// Virtual IOMMU ID (one per emulated SMMU instance).
    viommu_id: u32,
    /// S2 parent HWPT ID (nesting parent, linked to IOAS). Provides GPA→HPA
    /// translation; it is the nesting parent of every nested HWPT below, and
    /// is reached indirectly (via the bypass HWPT), not by direct attach.
    s2_parent_hwpt_id: u32,
    /// Shared, persistent nested HWPT with an abort STE (`Config=0b000`).
    /// Devices in ABORT attach here (rather than detaching), staying vIOMMU
    /// members. One per vIOMMU.
    abort_hwpt_id: u32,
    /// Shared, persistent nested HWPT with a bypass STE (`Config=0b100`: S1
    /// bypass over the S2 parent). Devices in BYPASS attach here for identity
    /// GPA→HPA. One per vIOMMU.
    bypass_hwpt_id: u32,
}

impl SmmuAccelState {
    /// Create per-SMMU iommufd objects.
    ///
    /// `dev_id` is any device bound to this IOMMU. The iommufd kernel
    /// requires a device reference to determine which physical IOMMU
    /// backs the vIOMMU.
    ///
    /// `s2_parent_hwpt_id` is the S2 parent HWPT, previously allocated
    /// via `IOMMU_HWPT_ALLOC` with `NEST_PARENT`.
    pub fn new(ctx: Arc<IommufdCtx>, dev_id: u32, s2_parent_hwpt_id: u32) -> anyhow::Result<Self> {
        let viommu_id = ctx
            .viommu_alloc(
                vfio_sys::iommufd::IOMMU_VIOMMU_TYPE_ARM_SMMUV3,
                dev_id,
                s2_parent_hwpt_id,
            )
            .context("failed to allocate vIOMMU for accel SMMU")?;

        // Pre-allocate the persistent abort and bypass nested HWPTs under this
        // vIOMMU (matching QEMU). Every device is always attached to a nested
        // HWPT — abort, bypass, or a per-device S1 translate HWPT — so ABORT and
        // BYPASS attach to these shared HWPTs rather than detaching or attaching
        // to the raw S2 parent, keeping every device within the vIOMMU nesting
        // and fault domain. Only STE.V and STE.Config are set; all else RES0.
        let abort_hwpt_id = ctx
            .hwpt_alloc(
                0,
                dev_id,
                viommu_id,
                vfio_sys::iommufd::IOMMU_HWPT_DATA_ARM_SMMUV3,
                Some(&vfio_sys::iommufd::IommuHwptArmSmmuv3 {
                    ste: ABORT_STE_DWORDS,
                }),
            )
            .context("failed to allocate abort HWPT for accel SMMU")?;
        let bypass_hwpt_id = ctx
            .hwpt_alloc(
                0,
                dev_id,
                viommu_id,
                vfio_sys::iommufd::IOMMU_HWPT_DATA_ARM_SMMUV3,
                Some(&vfio_sys::iommufd::IommuHwptArmSmmuv3 {
                    ste: BYPASS_STE_DWORDS,
                }),
            )
            .context("failed to allocate bypass HWPT for accel SMMU")?;

        tracing::info!(
            viommu_id,
            s2_parent_hwpt_id,
            abort_hwpt_id,
            bypass_hwpt_id,
            "created SMMU accel state (vIOMMU)"
        );

        Ok(Self {
            ctx,
            viommu_id,
            s2_parent_hwpt_id,
            abort_hwpt_id,
            bypass_hwpt_id,
        })
    }

    /// Returns the vIOMMU ID.
    pub fn viommu_id(&self) -> u32 {
        self.viommu_id
    }

    /// Returns the S2 parent HWPT ID (used for BYPASS mode attachment).
    pub fn s2_parent_hwpt_id(&self) -> u32 {
        self.s2_parent_hwpt_id
    }

    /// Returns the iommufd context.
    pub fn ctx(&self) -> &Arc<IommufdCtx> {
        &self.ctx
    }
}

/// Per-device iommufd stream backend for HW-accelerated nested S1.
///
/// Implements [`smmu::AcceleratedStreamBackend`], bridging SMMU CMDQ
/// commands to iommufd nested HWPT operations. One instance per VFIO
/// device behind an accel-capable SMMU.
///
/// # STE Config Handling
///
/// | STE.Config | Action |
/// |------------|--------|
/// | ABORT (0)  | Attach to the shared abort HWPT — DMA blocked |
/// | BYPASS (4) | Attach to the shared bypass HWPT — identity GPA→HPA via S2 |
/// | S1_TRANS (5) | Allocate a nested HWPT with STE DW0-1, attach (replace) |
///
/// # vDevice Allocation
///
/// The iommufd vDevice (virtual device within the vIOMMU) is allocated
/// lazily on first `on_cfgi_ste` with `Config=S1_TRANS`. The vDevice's
/// virtual stream ID is the guest-assigned BDF, which is not known at
/// device construction time (the guest assigns bus numbers after PCIe
/// enumeration).
pub struct IommufdStreamBackend {
    /// Per-SMMU shared state (vIOMMU, S2 parent HWPT).
    accel: Arc<SmmuAccelState>,
    /// iommufd device ID (from cdev bind).
    dev_id: u32,
    /// Shared VFIO device handle, used to issue
    /// `VFIO_DEVICE_ATTACH_IOMMUFD_PT` / `VFIO_DEVICE_DETACH_IOMMUFD_PT`.
    ///
    /// The same `Arc<vfio_sys::Device>` is held by the PCI emulation, so a
    /// single fd serves both roles (no dup).
    device: Arc<vfio_sys::Device>,
    /// Per-device mutable state (nested HWPT, vDevice).
    state: Mutex<StreamBackendState>,
}

/// Per-device mutable state for an [`IommufdStreamBackend`].
struct StreamBackendState {
    /// Whether the device is currently attached to a page table (one of the
    /// shared abort/bypass HWPTs or a per-device nested HWPT).
    ///
    /// The device starts detached (post-bind blocking domain); once the SMMU
    /// drives it to a policy it is always attached thereafter (attaches replace
    /// in place — we never detach on the live paths). This lets `Drop` know
    /// whether it must detach, and makes that detach a checked, fail-fast
    /// operation rather than a blind best-effort one.
    attached: bool,
    /// Current nested HWPT ID, if S1 translation is active. `None` when in
    /// ABORT or BYPASS (attached to the shared abort/bypass HWPT, which are
    /// owned by [`SmmuAccelState`], not tracked here).
    current_nested_hwpt: Option<u32>,
    /// vDevice ID, lazily allocated on first `CFGI_STE` with `S1_TRANS`.
    vdevice_id: Option<u32>,
}

impl IommufdStreamBackend {
    /// Create a new stream backend.
    ///
    /// `device` is the shared VFIO device handle (bound to iommufd), also held
    /// by the PCI emulation — one fd serves both.
    ///
    /// The device is detached (kernel blocking domain) immediately after bind.
    /// SMMU registration first attaches the shared abort HWPT. The backend
    /// remains aborting until PCI routing publishes the guest RequesterID and
    /// the SMMU applies that stream's current policy. After this first attach,
    /// the device is always attached to some nested HWPT and is never detached
    /// again until teardown.
    pub fn new(accel: Arc<SmmuAccelState>, dev_id: u32, device: Arc<vfio_sys::Device>) -> Self {
        Self {
            accel,
            dev_id,
            device,
            state: Mutex::new(StreamBackendState {
                attached: false,
                current_nested_hwpt: None,
                vdevice_id: None,
            }),
        }
    }

    /// Destroy an iommufd object this backend allocated, **failing fast** on
    /// error — on every path, including [`Drop`].
    ///
    /// A destroy failure is an internal-invariant violation, not
    /// guest-controllable: either the id is stale (already destroyed — our
    /// bookkeeping is wrong) or the object is unexpectedly still referenced
    /// (`EBUSY`). Continuing would leak kernel objects and leave the attach
    /// model in a state we can no longer reason about, so per the crate's
    /// fail-fast philosophy we panic rather than swallow it. (If this runs
    /// while already unwinding, the resulting abort is acceptable — we are
    /// terminating on a bug regardless.)
    fn destroy_owned(&self, id: u32, kind: &str) {
        self.accel
            .ctx
            .destroy(id)
            .unwrap_or_else(|e| panic!("smmu accel: failed to destroy {kind} {id:#x}: {e:#}"));
    }

    /// Attach the device to `pt_id`, replacing any current attachment, and
    /// record that the device is now attached.
    ///
    /// `attach_pt` performs an atomic HWPT replacement when the device is
    /// already attached, so callers never detach first.
    fn attach(&self, state: &mut StreamBackendState, pt_id: u32, what: &str) -> anyhow::Result<()> {
        self.device
            .attach_pt(pt_id)
            .with_context(|| format!("failed to attach device to {what}"))?;
        state.attached = true;
        Ok(())
    }

    /// Handle STE Config=ABORT: attach to the shared abort HWPT.
    ///
    /// Rather than detaching (which would drop the device to the kernel
    /// blocking domain, outside the vIOMMU), the device is attached to the
    /// shared nested abort HWPT (`Config=0b000`). `attach_pt` replaces the
    /// current attachment atomically, so the device is never left unattached.
    fn handle_abort(&self, state: &mut StreamBackendState) -> anyhow::Result<()> {
        self.attach(state, self.accel.abort_hwpt_id, "abort HWPT")?;

        // Destroy the previous per-device nested S1 HWPT, if any.
        if let Some(old_hwpt) = state.current_nested_hwpt.take() {
            self.destroy_owned(old_hwpt, "nested HWPT");
        }

        tracing::debug!(dev_id = self.dev_id, "SMMU accel: STE → ABORT (abort HWPT)");
        Ok(())
    }

    /// Handle STE Config=BYPASS: attach to the shared bypass HWPT.
    ///
    /// The bypass HWPT is a nested HWPT with a bypass STE (S1 bypass over the
    /// S2 parent), giving identity GPA→HPA while keeping the device a vIOMMU
    /// member. `attach_pt` replaces the current attachment atomically.
    fn handle_bypass(&self, state: &mut StreamBackendState) -> anyhow::Result<()> {
        self.attach(state, self.accel.bypass_hwpt_id, "bypass HWPT")?;

        // Destroy the previous per-device nested S1 HWPT, if any.
        if let Some(old_hwpt) = state.current_nested_hwpt.take() {
            self.destroy_owned(old_hwpt, "nested HWPT");
        }

        tracing::debug!(
            dev_id = self.dev_id,
            "SMMU accel: STE → BYPASS (bypass HWPT)"
        );
        Ok(())
    }

    /// Handle STE Config=S1_TRANS: allocate nested HWPT, attach device.
    fn handle_s1_translate(
        &self,
        state: &mut StreamBackendState,
        nested_ste: [u64; 2],
        stream_id: u32,
    ) -> anyhow::Result<()> {
        // Lazy vDevice allocation — the virtual stream ID is the guest-assigned
        // BDF from the CFGI_STE command's SID, not known at construction time.
        if state.vdevice_id.is_none() {
            let vdev_id = self
                .accel
                .ctx
                .vdevice_alloc(self.accel.viommu_id, self.dev_id, stream_id as u64)
                .with_context(|| {
                    format!(
                        "failed to allocate vDevice for dev_id={}, vsid={}",
                        self.dev_id, stream_id
                    )
                })?;
            tracing::info!(
                dev_id = self.dev_id,
                vdevice_id = vdev_id,
                virtual_sid = stream_id,
                "allocated iommufd vDevice"
            );
            state.vdevice_id = Some(vdev_id);
        }

        // The STE the kernel reads to program nested stage-1 translation.
        // `nested_ste` is already canonicalized by the SMMU emulator to the
        // stage-1 fields meaningful under its advertised capabilities (RES0 and
        // stage-2/override bits zeroed). That canonical form is exactly what
        // the Linux arm-smmu-v3 nesting path accepts — it rejects stray
        // reserved/override bits with `-EIO` — so no masking is needed here.
        let ste_data = vfio_sys::iommufd::IommuHwptArmSmmuv3 { ste: nested_ste };

        tracing::debug!(
            dev_id = self.dev_id,
            ste_dw0 = format_args!("{:#018x}", nested_ste[0]),
            ste_dw1 = format_args!("{:#018x}", nested_ste[1]),
            "SMMU accel: allocating nested HWPT with STE data"
        );

        // Allocate a new nested HWPT under the vIOMMU.
        let new_hwpt = self
            .accel
            .ctx
            .hwpt_alloc(
                0, // flags: not a nest parent
                self.dev_id,
                self.accel.viommu_id, // parent is the vIOMMU
                vfio_sys::iommufd::IOMMU_HWPT_DATA_ARM_SMMUV3,
                Some(&ste_data),
            )
            .context("failed to allocate nested HWPT for S1_TRANS")?;

        // Attach to the new nested HWPT. `attach` replaces the current
        // attachment (the shared abort/bypass HWPT, or an old per-device nested
        // HWPT) atomically, so the device is never transiently detached.
        // Replacement is atomic: on failure the old HWPT remains attached.
        // Destroy the unattached candidate before returning so both backend
        // state and the SMMU's forwarding state continue to describe the old
        // translation exactly.
        if let Err(e) = self.attach(state, new_hwpt, "nested HWPT") {
            self.destroy_owned(new_hwpt, "unattached nested HWPT");
            return Err(e);
        }

        // Destroy the old nested HWPT (if any).
        if let Some(old_hwpt) = state.current_nested_hwpt.replace(new_hwpt) {
            self.destroy_owned(old_hwpt, "nested HWPT");
        }

        tracing::debug!(
            dev_id = self.dev_id,
            nested_hwpt = new_hwpt,
            "SMMU accel: STE → S1_TRANS (nested HWPT)"
        );
        Ok(())
    }
}

impl smmu::AcceleratedStreamBackend for IommufdStreamBackend {
    fn set_stream_id(&self, sid: Option<u32>) -> anyhow::Result<()> {
        let mut state = self.state.lock();

        // Stop DMA under the old identity before retiring any object that an
        // incoming transaction or invalidation could still reference.
        self.handle_abort(&mut state)?;
        if let Some(vdevice_id) = state.vdevice_id.take() {
            self.destroy_owned(vdevice_id, "vDevice");
        }

        tracing::debug!(
            dev_id = self.dev_id,
            virtual_sid = sid,
            "SMMU accel: rebound StreamID"
        );
        Ok(())
    }

    fn set_stream_config(&self, config: smmu::StreamConfig) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        match config {
            smmu::StreamConfig::Abort => self.handle_abort(&mut state),
            smmu::StreamConfig::Bypass => self.handle_bypass(&mut state),
            // `ste_dwords` is already canonicalized by the emulator to the
            // stage-1 fields the host nesting path accepts; pass it through.
            smmu::StreamConfig::Translate { sid, ste_dwords } => {
                self.handle_s1_translate(&mut state, ste_dwords, sid)
            }
        }
    }
}

impl smmu::AcceleratedInvalidationSink for SmmuAccelState {
    fn invalidate(&self, entries: &[[u64; 2]]) -> Result<(), usize> {
        // Forward the batch of raw 128-bit CMDQ entries to the host as a single
        // ordered `IOMMU_HWPT_INVALIDATE` on this vIOMMU. Each entry is a
        // little-endian `[qw0, qw1]` quadword pair, exactly the layout the
        // kernel's ARM SMMUv3 invalidate command expects, so the emulator's
        // batch buffer is forwarded directly with no copy.
        match self.ctx.hwpt_invalidate(
            self.viommu_id,
            vfio_sys::iommufd::IOMMU_VIOMMU_INVALIDATE_DATA_ARM_SMMUV3,
            entries,
        ) {
            Ok(_handled) => Ok(()),
            Err(e) => {
                let handled = e.handled as usize;
                tracelimit::warn_ratelimited!(
                    error = &e as &dyn std::error::Error,
                    "smmu accel: host rejected invalidation batch"
                );
                // The kernel reports `handled` as the number of leading entries
                // it accepted, so the entry at that index is the offender. But
                // an early kernel failure (e.g. -ENOMEM) leaves the in/out count
                // at the input length without handling anything; in that case
                // `handled >= entries.len()` is meaningless. Fall back to the
                // start of the batch — re-presenting the (idempotent) prior
                // invalidations is safe, whereas advancing past unhandled
                // commands would drop invalidations and leave a stale host TLB.
                let failed_index = if handled < entries.len() { handled } else { 0 };
                Err(failed_index)
            }
        }
    }
}

impl Drop for IommufdStreamBackend {
    fn drop(&mut self) {
        // Take the tracked state first so the `state` borrow is released before
        // the destroy helper (which borrows `self`) runs.
        let (attached, nested_hwpt, vdevice) = {
            let state = self.state.get_mut();
            (
                state.attached,
                state.current_nested_hwpt.take(),
                state.vdevice_id.take(),
            )
        };

        // Detach only when we know the device is attached. Because attachment
        // is tracked precisely, a detach failure here is an invariant
        // violation — fail fast, like the destroys below.
        if attached {
            self.device.detach_pt().unwrap_or_else(|e| {
                panic!("smmu accel: failed to detach device on teardown: {e:#}")
            });
        }

        // Destroying objects we allocated must succeed; a failure is the same
        // invariant violation as on the live paths, so fail fast here too.
        if let Some(hwpt_id) = nested_hwpt {
            self.destroy_owned(hwpt_id, "nested HWPT");
        }
        if let Some(vdev_id) = vdevice {
            self.destroy_owned(vdev_id, "vDevice");
        }
    }
}
