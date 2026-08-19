// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! iommufd nested translation for VFIO devices behind an accel-capable SMMU.
//!
//! This module implements HW-accelerated nested stage 1 translation using
//! iommufd. The guest programs the emulated SMMU's stream table entries (STEs)
//! and page tables. The SMMU emulator decodes the guest's CMDQ commands and
//! dispatches a [`smmu::StreamConfig`] to this module via the per-device
//! [`smmu::AcceleratedStreamBackend`] trait, and forwards batched invalidation
//! commands via the per-vIOMMU [`smmu::Invalidate`] trait,
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
//!   SMMU. Implements [`smmu::Invalidate`]: invalidation is
//!   vIOMMU-scoped, so one batched `IOMMU_HWPT_INVALIDATE` per guest command
//!   covers every stream behind the SMMU.
//! - [`IommufdStreamBackend`]: per-device stream backend, created during VFIO
//!   cdev device resolution and attached to the abort HWPT straight away, so a
//!   device is a vIOMMU member before it has any StreamID.
//! - [`AccelStream`]: the device's participation in the emulated SMMU. PCI
//!   routing owns the guest-assigned BDF, so this owns the StreamID derived
//!   from it, plus the [`VDevice`] and SMMU registration keyed by it. A guest
//!   moving the device to a different BDF retires all three and creates new
//!   ones, which is why the SMMU never has to be told a StreamID changed.

use anyhow::Context as _;
use pal_async::driver::PollImpl;
use pal_async::driver::SpawnDriver;
use pal_async::fd::PollFdReady;
use pal_async::interest::InterestSlot;
use pal_async::interest::PollEvents;
use pal_async::task::Spawn;
use pal_async::task::Task;
use parking_lot::Mutex;
use std::fs::File;
use std::io;
use std::io::Read as _;
use std::os::fd::AsRawFd as _;
use std::sync::Arc;
use std::sync::Weak;
use std::task::Poll;
use vfio_sys::iommufd::IommuVeventArmSmmuv3;
use vfio_sys::iommufd::IommufdCtx;
use vfio_sys::iommufd::IommufdVeventHeader;
use vfio_sys::iommufd::ViommuAlloc;
use zerocopy::FromBytes;

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

/// Owns a newly allocated iommufd object until its ID is transferred into
/// long-lived state. Uncommitted objects are destroyed on scope exit.
struct PendingIommufdObject<'a> {
    ctx: &'a IommufdCtx,
    id: Option<u32>,
    kind: &'static str,
}

impl<'a> PendingIommufdObject<'a> {
    fn new(ctx: &'a IommufdCtx, id: u32, kind: &'static str) -> Self {
        Self {
            ctx,
            id: Some(id),
            kind,
        }
    }

    fn id(&self) -> u32 {
        self.id.expect("pending iommufd object must have an ID")
    }

    fn into_id(mut self) -> u32 {
        self.id
            .take()
            .expect("pending iommufd object must have an ID")
    }

    fn replace(mut self, slot: &mut Option<u32>) {
        std::mem::swap(&mut self.id, slot);
    }
}

impl Drop for PendingIommufdObject<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.ctx.destroy(id).unwrap_or_else(|e| {
                panic!(
                    "smmu accel: failed to destroy pending {} {id:#x}: {e:#}",
                    self.kind
                )
            });
        }
    }
}

/// A nesting parent (stage-2) HWPT under an IOAS.
///
/// This is the hardware realization of its IOAS's GPA→HPA map — every
/// `IOAS_MAP` is replayed into it — and the nesting parent of every vIOMMU
/// built on top. The kernel allocates the underlying domain from the IOMMU
/// driver of whichever device asked for it, so one parent serves only devices
/// on that same physical IOMMU; a host with several IOMMU instances needs one
/// parent each. Hence a set of these per IOAS rather than a single ID, matching
/// QEMU's `VFIOIOASHwpt` list.
///
/// Refcounted by the [`SmmuAccelState`]s nesting on it, which are in turn
/// refcounted by their devices, so the last device out destroys the chain.
pub struct NestingParent {
    ctx: Arc<IommufdCtx>,
    hwpt_id: u32,
}

impl NestingParent {
    /// Allocates a nesting parent for `ioas_id` from `dev_id`'s IOMMU.
    pub fn new(ctx: Arc<IommufdCtx>, dev_id: u32, ioas_id: u32) -> anyhow::Result<Self> {
        let hwpt_id = ctx
            .hwpt_alloc(
                vfio_sys::iommufd::IOMMU_HWPT_ALLOC_NEST_PARENT,
                dev_id,
                ioas_id,
                vfio_sys::iommufd::IOMMU_HWPT_DATA_NONE,
                None,
            )
            .context("failed to allocate S2 parent HWPT for nesting")?;
        tracing::debug!(ioas_id, hwpt_id, dev_id, "allocated nesting parent HWPT");
        Ok(Self { ctx, hwpt_id })
    }

    /// Attempts to build a vIOMMU for `dev_id` nesting on this parent.
    ///
    /// Returns [`ViommuAlloc::Incompatible`] if this parent belongs to a
    /// different physical SMMU than `dev_id`.
    pub fn alloc_viommu(&self, dev_id: u32) -> anyhow::Result<ViommuAlloc> {
        self.ctx.viommu_alloc(
            vfio_sys::iommufd::IOMMU_VIOMMU_TYPE_ARM_SMMUV3,
            dev_id,
            self.hwpt_id,
        )
    }
}

impl Drop for NestingParent {
    fn drop(&mut self) {
        self.ctx.destroy(self.hwpt_id).unwrap_or_else(|e| {
            panic!(
                "smmu accel: failed to destroy nesting parent HWPT {:#x}: {e:#}",
                self.hwpt_id
            )
        });
    }
}

/// The host vIOMMU standing for one emulated SMMU.
///
/// Refcounted separately from [`SmmuAccelState`] so the vEVENTQ poll task can
/// hold it: the queue is a kernel object nested under the vIOMMU and keeps a
/// reference on it, so the vIOMMU cannot be destroyed until the task has closed
/// the queue fd and destroyed the queue.
struct Viommu {
    ctx: Arc<IommufdCtx>,
    /// Keeps the nesting parent alive for at least as long as this vIOMMU.
    _parent: Arc<NestingParent>,
    id: u32,
}

impl Drop for Viommu {
    fn drop(&mut self) {
        self.ctx.destroy(self.id).unwrap_or_else(|e| {
            panic!("smmu accel: failed to destroy vIOMMU {:#x}: {e:#}", self.id)
        });
        tracing::info!(viommu_id = self.id, "destroyed vIOMMU");
    }
}

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
    /// The host vIOMMU every object below is allocated under.
    viommu: Arc<Viommu>,
    /// Shared, persistent nested HWPT with an abort STE (`Config=0b000`).
    /// Devices in ABORT attach here (rather than detaching), staying vIOMMU
    /// members. One per vIOMMU.
    abort_hwpt_id: u32,
    /// Shared, persistent nested HWPT with a bypass STE (`Config=0b100`: S1
    /// bypass over the S2 parent). Devices in BYPASS attach here for identity
    /// GPA→HPA. One per vIOMMU.
    bypass_hwpt_id: u32,
    /// Forwards host SMMU faults for this vIOMMU's streams into the guest's
    /// Event queue. Dropping the handle cancels the task, which then tears the
    /// queue down in order and releases its own vIOMMU reference.
    _veventq: Task<()>,
}

impl SmmuAccelState {
    /// Create per-SMMU iommufd objects on `viommu_id`, a vIOMMU the caller has
    /// already allocated on `parent` via [`NestingParent::alloc_viommu`].
    ///
    /// Takes ownership of `viommu_id`: it is destroyed if this fails, and with
    /// this state otherwise.
    pub fn new(
        ctx: Arc<IommufdCtx>,
        dev_id: u32,
        parent: Arc<NestingParent>,
        viommu_id: u32,
        vsmmu: &Arc<smmu::SmmuSharedState>,
        driver: &dyn SpawnDriver,
    ) -> anyhow::Result<Self> {
        let s2_parent_hwpt_id = parent.hwpt_id;
        // Take ownership of the vIOMMU before allocating anything under it, so
        // that the locals below are declared parent-first and therefore drop
        // children-first on every error path.
        let viommu = Arc::new(Viommu {
            ctx: ctx.clone(),
            _parent: parent,
            id: viommu_id,
        });

        // Pre-allocate the persistent abort and bypass nested HWPTs under this
        // vIOMMU (matching QEMU). Every device is always attached to a nested
        // HWPT — abort, bypass, or a per-device S1 translate HWPT — so ABORT and
        // BYPASS attach to these shared HWPTs rather than detaching or attaching
        // to the raw S2 parent, keeping every device within the vIOMMU nesting
        // and fault domain. Only STE.V and STE.Config are set; all else RES0.
        let abort_hwpt = PendingIommufdObject::new(
            &ctx,
            ctx.hwpt_alloc(
                0,
                dev_id,
                viommu.id,
                vfio_sys::iommufd::IOMMU_HWPT_DATA_ARM_SMMUV3,
                Some(&vfio_sys::iommufd::IommuHwptArmSmmuv3 {
                    ste: ABORT_STE_DWORDS,
                }),
            )
            .context("failed to allocate abort HWPT for accel SMMU")?,
            "abort HWPT",
        );
        let bypass_hwpt = PendingIommufdObject::new(
            &ctx,
            ctx.hwpt_alloc(
                0,
                dev_id,
                viommu.id,
                vfio_sys::iommufd::IOMMU_HWPT_DATA_ARM_SMMUV3,
                Some(&vfio_sys::iommufd::IommuHwptArmSmmuv3 {
                    ste: BYPASS_STE_DWORDS,
                }),
            )
            .context("failed to allocate bypass HWPT for accel SMMU")?,
            "bypass HWPT",
        );

        tracing::debug!(
            viommu_id = viommu.id,
            s2_parent_hwpt_id,
            abort_hwpt_id = abort_hwpt.id(),
            bypass_hwpt_id = bypass_hwpt.id(),
            "created SMMU accel state (vIOMMU)"
        );

        let veventq = spawn_veventq(&ctx, &viommu, vsmmu, driver)?;
        let abort_hwpt_id = abort_hwpt.into_id();
        let bypass_hwpt_id = bypass_hwpt.into_id();

        Ok(Self {
            ctx,
            viommu,
            abort_hwpt_id,
            bypass_hwpt_id,
            _veventq: veventq,
        })
    }
}

impl SmmuAccelState {
    /// Allocates the vDevice binding `vsid` to `dev_id` within this vIOMMU.
    ///
    /// The host keys its invalidation routing by this (vIOMMU, vSID) pair, so
    /// exactly one may exist per StreamID at a time.
    pub fn alloc_vdevice(&self, dev_id: u32, vsid: u32) -> anyhow::Result<VDevice> {
        let id = self
            .ctx
            .vdevice_alloc(self.viommu.id, dev_id, vsid.into())
            .with_context(|| {
                format!("failed to allocate vDevice for dev_id={dev_id}, vsid={vsid:#x}")
            })?;
        tracing::debug!(
            dev_id,
            vdevice_id = id,
            virtual_sid = vsid,
            "allocated iommufd vDevice"
        );
        Ok(VDevice {
            ctx: self.ctx.clone(),
            id,
        })
    }
}

/// The iommufd vDevice binding one guest StreamID to a device within a vIOMMU.
///
/// Owned by whoever owns the StreamID, so the host binding is destroyed exactly
/// when the guest stops using that identity.
pub struct VDevice {
    ctx: Arc<IommufdCtx>,
    id: u32,
}

impl Drop for VDevice {
    fn drop(&mut self) {
        self.ctx.destroy(self.id).unwrap_or_else(|e| {
            panic!(
                "smmu accel: failed to destroy vDevice {:#x}: {e:#}",
                self.id
            )
        });
    }
}

impl Drop for SmmuAccelState {
    fn drop(&mut self) {
        // Runs once the cache entry and every stream backend behind this vSMMU
        // are gone, so each backend has already destroyed its own nested HWPT
        // and vDevice. These are the last children of the vIOMMU this state
        // owns directly; the vIOMMU itself outlives them, and outlives the
        // vEVENTQ task too, until the last `Arc<Viommu>` goes.
        for (id, kind) in [
            (self.bypass_hwpt_id, "bypass HWPT"),
            (self.abort_hwpt_id, "abort HWPT"),
        ] {
            self.ctx
                .destroy(id)
                .unwrap_or_else(|e| panic!("smmu accel: failed to destroy {kind} {id:#x}: {e:#}"));
        }
        tracing::debug!(viommu_id = self.viommu.id, "destroyed SMMU accel state");
    }
}

/// Host-side buffering for physical SMMU events awaiting forwarding.
///
/// This absorbs event bursts and userspace scheduling latency independently
/// of the guest's Event queue, which also receives emulated SMMU events.
const HOST_VEVENTQ_DEPTH: u32 = 256;

/// vEVENT sequence numbers are monotonic over `[0, i32::MAX]`, wrapping to 0.
const VEVENT_SEQUENCE_MODULUS: u32 = i32::MAX as u32 + 1;

/// A header plus one ARM SMMUv3 record. The kernel only ever writes whole
/// records, so a read buffer sized in multiples of this is never truncated.
const VEVENT_RECORD_LEN: usize =
    size_of::<IommufdVeventHeader>() + size_of::<IommuVeventArmSmmuv3>();

/// How many records to drain per read.
const VEVENT_BATCH_LEN: usize = VEVENT_RECORD_LEN * 16;

/// An iommufd object destroyed when this handle drops.
struct OwnedIommufdObject {
    ctx: Arc<IommufdCtx>,
    id: u32,
    kind: &'static str,
}

impl Drop for OwnedIommufdObject {
    fn drop(&mut self) {
        self.ctx.destroy(self.id).unwrap_or_else(|e| {
            panic!(
                "smmu accel: failed to destroy {} {:#x}: {e:#}",
                self.kind, self.id
            )
        });
    }
}

/// One vIOMMU's virtual event queue: the host's channel for SMMU faults taken
/// by streams that translate in hardware, and so are invisible to the emulator.
///
/// Field order is load-bearing, because teardown has to run inwards: the
/// readiness registration (which unregisters the fd, and so must precede the
/// close), then the fd (which holds a kernel reference on the queue), then the
/// queue object, and only then this reference to the vIOMMU it nests under.
struct VEventQ {
    fd_ready: PollImpl<dyn PollFdReady>,
    file: File,
    _object: OwnedIommufdObject,
    _viommu: Arc<Viommu>,
}

impl VEventQ {
    /// Waits for and reads the next batch of whole event records.
    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        std::future::poll_fn(|cx| {
            loop {
                // Clear readiness *before* reading, so an event arriving after
                // the read leaves the fd marked ready rather than having its
                // wakeup clobbered by a later clear.
                self.fd_ready.clear_fd_ready(InterestSlot::Read);
                match (&self.file).read(buf) {
                    // An empty queue reads as zero bytes rather than failing
                    // with `EAGAIN`; the fd never reaches end of file.
                    Ok(0) => {}
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    r => return Poll::Ready(r),
                }
                std::task::ready!(self.fd_ready.poll_fd_ready(
                    cx,
                    InterestSlot::Read,
                    PollEvents::IN
                ));
            }
        })
        .await
    }
}

/// Allocates this vIOMMU's virtual event queue and spawns the task forwarding
/// its records into the guest's Event queue.
fn spawn_veventq(
    ctx: &Arc<IommufdCtx>,
    viommu: &Arc<Viommu>,
    vsmmu: &Arc<smmu::SmmuSharedState>,
    driver: &dyn SpawnDriver,
) -> anyhow::Result<Task<()>> {
    let (queue_id, file) = ctx
        .veventq_alloc(
            viommu.id,
            vfio_sys::iommufd::IOMMU_VEVENTQ_TYPE_ARM_SMMUV3,
            HOST_VEVENTQ_DEPTH,
        )
        .context("failed to allocate vEVENTQ for accel SMMU")?;
    let pending = PendingIommufdObject::new(ctx, queue_id, "vEVENTQ");
    let fd_ready = match driver.new_dyn_fd_ready(file.as_raw_fd()) {
        Ok(fd_ready) => fd_ready,
        Err(err) => {
            // Close the fd before `pending` destroys the queue: the fd holds a
            // kernel reference that would make the destroy fail.
            drop(file);
            return Err(err).context("failed to register the vEVENTQ fd for polling");
        }
    };
    let queue = VEventQ {
        fd_ready,
        file,
        _object: OwnedIommufdObject {
            ctx: ctx.clone(),
            id: pending.into_id(),
            kind: "vEVENTQ",
        },
        _viommu: viommu.clone(),
    };
    tracing::info!(viommu_id = viommu.id, queue_id, "allocated vEVENTQ");
    Ok(Spawn::spawn(
        &driver,
        "smmu-veventq",
        forward_veventq(queue, Arc::downgrade(vsmmu)),
    ))
}

/// Forwards host SMMU events for this vIOMMU's streams into the guest's Event
/// queue until the emulated SMMU goes away or the task is cancelled.
async fn forward_veventq(mut queue: VEventQ, vsmmu: Weak<smmu::SmmuSharedState>) {
    let mut buf = [0; VEVENT_BATCH_LEN];
    let mut last_sequence = None;
    loop {
        let len = match queue.read(&mut buf).await {
            Ok(len) => len,
            Err(err) => {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "smmu accel: vEVENTQ read failed; host events can no longer reach the guest"
                );
                return;
            }
        };
        let Some(vsmmu) = vsmmu.upgrade() else {
            return;
        };
        let mut stream = &buf[..len];
        while let Some(event) = next_vevent(&mut stream, &mut last_sequence) {
            if event.lost > 0 {
                tracelimit::warn_ratelimited!(
                    lost = event.lost,
                    "smmu accel: host vEVENTQ events lost"
                );
            }
            if let Some(record) = event.record {
                vsmmu.record_accel_event(record);
            }
        }
    }
}

/// One entry decoded from the vEVENTQ stream.
struct VEvent {
    /// How many records the host discarded immediately before this one.
    lost: u32,
    /// The event record, absent when the header only reports loss.
    record: Option<[u64; 4]>,
}

/// Pops the next entry off the vEVENTQ stream, returning `None` once it is
/// exhausted or found to be truncated.
///
/// The stream is a packed run of headers, each followed by an ARM SMMUv3 record
/// unless it carries `LOST_EVENTS`. Every reported event consumes a sequence
/// number, including ones the host had no room to queue, so a gap between
/// adjacent headers counts exactly the records lost in between.
fn next_vevent(stream: &mut &[u8], last_sequence: &mut Option<u32>) -> Option<VEvent> {
    if stream.is_empty() {
        return None;
    }
    let (header, rest) = IommufdVeventHeader::read_from_prefix(stream)
        .inspect_err(|_| {
            tracelimit::warn_ratelimited!(len = stream.len(), "smmu accel: truncated vEVENT header")
        })
        .ok()?;
    *stream = rest;

    let gap = last_sequence.map_or(0, |last| {
        (header.sequence.wrapping_sub(last) % VEVENT_SEQUENCE_MODULUS).saturating_sub(1)
    });
    *last_sequence = Some(header.sequence);

    // A `LOST_EVENTS` header stands in for a record the host could not queue,
    // and carries no data of its own.
    if header.flags & vfio_sys::iommufd::IOMMU_VEVENTQ_FLAG_LOST_EVENTS != 0 {
        return Some(VEvent {
            lost: gap + 1,
            record: None,
        });
    }

    let (event, rest) = IommuVeventArmSmmuv3::read_from_prefix(stream)
        .inspect_err(|_| {
            tracelimit::warn_ratelimited!(len = stream.len(), "smmu accel: truncated vEVENT record")
        })
        .ok()?;
    *stream = rest;
    Some(VEvent {
        lost: gap,
        record: Some(event.evt),
    })
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
/// The backend is registered against one StreamID for its whole life, so it
/// never has to interpret one: the [`VDevice`] naming that StreamID is owned by
/// [`AccelStream`] and outlives every config applied here.
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
    /// Per-device mutable state (attachment and nested HWPT).
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
}

impl IommufdStreamBackend {
    /// Create a new stream backend.
    ///
    /// `device` is the shared VFIO device handle (bound to iommufd), also held
    /// by the PCI emulation — one fd serves both.
    ///
    /// The device is detached (kernel blocking domain) immediately after bind
    /// and stays that way until [`attach_abort`](Self::attach_abort). After
    /// that first attach it is always attached to some nested HWPT, and is
    /// never detached again until teardown.
    pub fn new(accel: Arc<SmmuAccelState>, dev_id: u32, device: Arc<vfio_sys::Device>) -> Self {
        Self {
            accel,
            dev_id,
            device,
            state: Mutex::new(StreamBackendState {
                attached: false,
                current_nested_hwpt: None,
            }),
        }
    }

    /// Attaches the shared abort HWPT, blocking DMA.
    ///
    /// Used at device resolution to make the device a vIOMMU member before the
    /// guest has assigned it a BDF, and so a StreamID.
    pub fn attach_abort(&self) -> anyhow::Result<()> {
        self.handle_abort(&mut self.state.lock())
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
    ) -> anyhow::Result<()> {
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
        let new_hwpt = PendingIommufdObject::new(
            &self.accel.ctx,
            self.accel
                .ctx
                .hwpt_alloc(
                    0, // flags: not a nest parent
                    self.dev_id,
                    self.accel.viommu.id, // parent is the vIOMMU
                    vfio_sys::iommufd::IOMMU_HWPT_DATA_ARM_SMMUV3,
                    Some(&ste_data),
                )
                .context("failed to allocate nested HWPT for S1_TRANS")?,
            "nested HWPT",
        );

        // Attach to the new nested HWPT. `attach` replaces the current
        // attachment (the shared abort/bypass HWPT, or an old per-device nested
        // HWPT) atomically, so the device is never transiently detached.
        // Replacement is atomic: on failure the old HWPT remains attached and
        // the pending candidate is destroyed on scope exit.
        self.attach(state, new_hwpt.id(), "nested HWPT")?;

        let new_hwpt_id = new_hwpt.id();
        new_hwpt.replace(&mut state.current_nested_hwpt);

        tracing::debug!(
            dev_id = self.dev_id,
            nested_hwpt = new_hwpt_id,
            "SMMU accel: STE → S1_TRANS (nested HWPT)"
        );
        Ok(())
    }
}

impl smmu::AcceleratedStreamBackend for IommufdStreamBackend {
    fn set_stream_config(&self, config: smmu::StreamConfig) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        match config {
            smmu::StreamConfig::Abort => self.handle_abort(&mut state),
            smmu::StreamConfig::Bypass => self.handle_bypass(&mut state),
            // `ste_dwords` is already canonicalized by the emulator to the
            // stage-1 fields the host nesting path accepts; pass it through.
            smmu::StreamConfig::Translate { ste_dwords } => {
                self.handle_s1_translate(&mut state, ste_dwords)
            }
        }
    }
}

impl smmu::Invalidate for SmmuAccelState {
    fn invalidate(&self, entries: &[[u64; 2]]) -> Result<(), usize> {
        // Forward the batch of raw 128-bit CMDQ entries to the host as a single
        // ordered `IOMMU_HWPT_INVALIDATE` on this vIOMMU. Each entry is a
        // little-endian `[qw0, qw1]` quadword pair, exactly the layout the
        // kernel's ARM SMMUv3 invalidate command expects, so the emulator's
        // batch buffer is forwarded directly with no copy.
        match self.ctx.hwpt_invalidate(
            self.viommu.id,
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
        let (attached, nested_hwpt) = {
            let state = self.state.get_mut();
            (state.attached, state.current_nested_hwpt.take())
        };

        // Detach only when we know the device is attached. Because attachment
        // is tracked precisely, a detach failure here is an invariant
        // violation — fail fast, like the destroy below.
        if attached {
            self.device.detach_pt().unwrap_or_else(|e| {
                panic!("smmu accel: failed to detach device on teardown: {e:#}")
            });
        }

        if let Some(hwpt_id) = nested_hwpt {
            self.destroy_owned(hwpt_id, "nested HWPT");
        }
    }
}

/// A VFIO device's accelerated stream: the host objects and SMMU registration
/// that make up its participation in the emulated SMMU.
///
/// PCI routing owns the guest-assigned BDF, so this owns the StreamID derived
/// from it and everything keyed by it. A device only appears in the SMMU's
/// registration table while it has one, which is why the SMMU never has to be
/// told that a StreamID arrived or changed.
#[derive(inspect::Inspect)]
pub struct AccelStream {
    /// Declared first so teardown releases the StreamID-scoped objects before
    /// the vIOMMU they live under: the last `Arc<SmmuAccelState>` here is
    /// dropped by the backend below, which destroys the vIOMMU.
    bound: Option<BoundStream>,
    #[inspect(skip)]
    accel: Arc<SmmuAccelState>,
    #[inspect(skip)]
    backend: Arc<IommufdStreamBackend>,
    #[inspect(skip)]
    shared: Arc<smmu::SmmuSharedState>,
    #[inspect(hex)]
    stream_id_base: u32,
    dev_id: u32,
}

/// The objects that exist only while the device holds a StreamID.
///
/// Dropped in declaration order: the SMMU registration first, which drives the
/// stream to abort under the SMMU's lock, and only then the vDevice that
/// StreamID named.
#[derive(inspect::Inspect)]
struct BoundStream {
    #[inspect(hex)]
    rid: u16,
    #[inspect(hex, with = "|r| r.stream_id()")]
    registration: smmu::AccelRegistration,
    #[inspect(skip)]
    #[expect(dead_code)]
    vdevice: VDevice,
}

impl AccelStream {
    /// Wires a freshly bound VFIO device into `nesting`'s emulated SMMU,
    /// leaving it attached to the abort HWPT.
    ///
    /// The device is a vIOMMU member from here on, but has no StreamID until
    /// the guest assigns it a BDF, so it stays blocked until then.
    pub fn new(
        nesting: &smmu::SmmuNestingContext,
        accel: Arc<SmmuAccelState>,
        dev_id: u32,
        device: Arc<vfio_sys::Device>,
    ) -> anyhow::Result<Self> {
        let backend = Arc::new(IommufdStreamBackend::new(accel.clone(), dev_id, device));
        backend
            .attach_abort()
            .context("failed to block DMA for a newly bound VFIO device")?;
        Ok(Self {
            bound: None,
            accel,
            backend,
            shared: nesting.shared.clone(),
            stream_id_base: nesting.stream_id_base,
            dev_id,
        })
    }

    /// Binds the device to the StreamID for `rid`, replacing any StreamID it
    /// already holds. Repeating the current RID is a no-op.
    ///
    /// On failure the device holds no StreamID and remains blocked; a later
    /// routed configuration write retries.
    pub fn bind(&mut self, rid: u16) -> anyhow::Result<()> {
        if self.bound.as_ref().is_some_and(|bound| bound.rid == rid) {
            return Ok(());
        }
        // Release the old StreamID before claiming the new one: the host allows
        // one vDevice per StreamID, and this device may be moving onto a
        // StreamID some other device is simultaneously moving off.
        self.unbind();

        let sid = self.stream_id_base + u32::from(rid);
        let vdevice = self.accel.alloc_vdevice(self.dev_id, sid)?;
        let registration = self
            .shared
            .register_accel_device(sid, self.backend.clone())?;
        self.bound = Some(BoundStream {
            rid,
            registration,
            vdevice,
        });
        Ok(())
    }

    /// Releases the device's StreamID, leaving it blocked. Used when a reset
    /// clears the captured BDF.
    pub fn unbind(&mut self) {
        self.bound = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;
    use zerocopy::IntoBytes;

    fn push_header(stream: &mut Vec<u8>, flags: u32, sequence: u32) {
        stream.extend_from_slice(IommufdVeventHeader { flags, sequence }.as_bytes());
    }

    fn push_record(stream: &mut Vec<u8>, flags: u32, sequence: u32, evt: [u64; 4]) {
        push_header(stream, flags, sequence);
        stream.extend_from_slice(IommuVeventArmSmmuv3 { evt }.as_bytes());
    }

    fn drain(stream: &[u8], last_sequence: &mut Option<u32>) -> Vec<(u32, Option<[u64; 4]>)> {
        let mut stream = stream;
        let mut out = Vec::new();
        while let Some(event) = next_vevent(&mut stream, last_sequence) {
            out.push((event.lost, event.record));
        }
        out
    }

    #[test]
    fn vevent_stream_yields_each_record() {
        let mut stream = Vec::new();
        push_record(&mut stream, 0, 0, [1, 2, 3, 4]);
        push_record(&mut stream, 0, 1, [5, 6, 7, 8]);

        assert_eq!(
            drain(&stream, &mut None),
            [(0, Some([1, 2, 3, 4])), (0, Some([5, 6, 7, 8]))]
        );
    }

    #[test]
    fn vevent_sequence_gap_counts_lost_records() {
        let mut stream = Vec::new();
        push_record(&mut stream, 0, 3, [0; 4]);
        // Sequences 4 and 5 never arrive, so two records were lost.
        push_record(&mut stream, 0, 6, [9; 4]);

        assert_eq!(
            drain(&stream, &mut None),
            [(0, Some([0; 4])), (2, Some([9; 4]))]
        );
    }

    #[test]
    fn vevent_sequence_wraps_at_i32_max() {
        let mut stream = Vec::new();
        push_record(&mut stream, 0, i32::MAX as u32, [0; 4]);
        push_record(&mut stream, 0, 0, [1; 4]);
        push_record(&mut stream, 0, 3, [2; 4]);

        assert_eq!(
            drain(&stream, &mut None),
            [(0, Some([0; 4])), (0, Some([1; 4])), (2, Some([2; 4]))]
        );
    }

    /// A trailing `LOST_EVENTS` header stands in for a record the host could
    /// not queue, and carries no data.
    #[test]
    fn vevent_lost_events_header_reports_loss_without_a_record() {
        let mut stream = Vec::new();
        push_record(&mut stream, 0, 0, [7; 4]);
        push_header(
            &mut stream,
            vfio_sys::iommufd::IOMMU_VEVENTQ_FLAG_LOST_EVENTS,
            2,
        );

        // Sequence 1 was lost as well as the record this header stands for.
        assert_eq!(drain(&stream, &mut None), [(0, Some([7; 4])), (2, None)]);
    }

    /// A read that ends mid-record must not be misparsed; the kernel only
    /// writes whole records, so this can only mean a malformed stream.
    #[test]
    fn vevent_truncated_record_stops_the_stream() {
        let mut stream = Vec::new();
        push_record(&mut stream, 0, 0, [7; 4]);
        push_header(&mut stream, 0, 1);

        assert_eq!(drain(&stream, &mut None), [(0, Some([7; 4]))]);
    }

    /// Loss accounting continues across reads.
    #[test]
    fn vevent_sequence_carries_across_reads() {
        let mut last_sequence = None;

        let mut first = Vec::new();
        push_record(&mut first, 0, 10, [0; 4]);
        assert_eq!(drain(&first, &mut last_sequence), [(0, Some([0; 4]))]);

        let mut second = Vec::new();
        push_record(&mut second, 0, 13, [1; 4]);
        assert_eq!(drain(&second, &mut last_sequence), [(2, Some([1; 4]))]);
    }
}
