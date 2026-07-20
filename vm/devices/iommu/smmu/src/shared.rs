// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared SMMU state and per-device translation wrappers.
//!
//! [`SmmuSharedState`] holds the SMMU configuration that per-device wrappers
//! need for translation: stream table base, CR0 state, and a reference to
//! guest memory for walking page tables.
//!
//! [`SmmuTranslator`] implements
//! [`IommuTranslator`](iommu_common::IommuTranslator), translating IOVAs to
//! GPAs via the SMMU page tables. The generic
//! [`TranslatingMemory`](iommu_common::TranslatingMemory) in `iommu_common`
//! provides the [`GuestMemoryAccess`] boilerplate.
//!
//! [`SmmuSignalMsi`] implements [`SignalMsi`], translating the MSI address
//! (which may be an IOVA) to a GPA before forwarding to the inner MSI
//! target.
//!
//! [`SmmuIrqFd`] implements [`IrqFd`](vmcore::irqfd::IrqFd), producing
//! [`SmmuIrqFdRoute`] instances that translate the MSI address on
//! [`enable`](vmcore::irqfd::IrqFdRoute::enable) before forwarding to the
//! inner irqfd route.

use crate::spec::events::EvtEntry;
use crate::spec::registers;
use crate::translate;
use guestmem::GuestMemory;
use pal_event::Event;
use parking_lot::Mutex;
use parking_lot::RwLock;
use pci_core::bus_range::AssignedBusRange;
use pci_core::msi::SignalMsi;
use std::sync::Arc;
use std::sync::OnceLock;
use vmcore::irqfd::IrqFd;
use vmcore::irqfd::IrqFdRoute;
use vmcore::line_interrupt::LineInterrupt;
use zerocopy::IntoBytes;

/// The context a host-assignment backend needs to wire a VFIO device into
/// this emulated SMMU for hardware-accelerated nested stage-1 translation.
///
/// This is the concrete type carried (type-erased) by
/// [`DmaPassthrough::HardwareNestable`](pci_core::dma::DmaPassthrough::HardwareNestable)
/// for devices behind an accel-capable SMMU. The VFIO resolver downcasts the
/// opaque handle to this type and runs the one-shot nesting handshake
/// (`resolve_host_caps` → build a stream backend → [`register_accel_device`]).
///
/// [`register_accel_device`]: SmmuSharedState::register_accel_device
#[derive(Clone)]
pub struct SmmuNestingContext {
    /// Shared state of the emulated SMMU this device sits behind.
    pub shared: Arc<SmmuSharedState>,
    /// The device's assigned bus range, used to derive its stream ID.
    pub bus_range: AssignedBusRange,
    /// Offset into the SMMU's stream table (0 for a 1:1 SMMU-per-root-complex
    /// topology).
    pub stream_id_base: u32,
}

/// Backend for a single VFIO device's stream, bridging SMMU CMDQ commands
/// to iommufd nested HWPT operations.
///
/// The SMMU emulator dispatches CMDQ commands to registered backends on a
/// per-stream-ID basis. Streams without a registered backend use the
/// software page table walk path (emulated devices). Streams with a backend
/// use hardware-accelerated translation via iommufd.
///
/// The SMMU emulator owns the SMMUv3 spec: it parses and validates the guest
/// STE and dispatches a decoded [`StreamConfig`] to the backend, which only
/// maps each variant onto host IOMMU operations.
///
/// This trait is per-device (one instance per VFIO device). Invalidation,
/// which is vIOMMU-scoped, lives on [`AcceleratedInvalidationSink`] instead.
pub trait AcceleratedStreamBackend: Send + Sync {
    /// The guest reconfigured this stream's STE (via `CFGI_STE`), or the
    /// emulator recomputed the stream's policy (e.g. on a `GBPA` write or
    /// `SMMUEN` transition). The emulator has already parsed and validated
    /// the STE into `config`. Only [`StreamConfig::Translate`] carries a
    /// stream ID (for lazy vDevice allocation); the bypass and abort cases
    /// have no per-stream identity to act on.
    fn set_stream_config(&self, config: StreamConfig) -> anyhow::Result<()>;
}

/// Sink that forwards a guest's invalidation commands to the host as a single
/// ordered, batched stream per emulated SMMU.
///
/// Invalidation is **vIOMMU-scoped**, not device-scoped: a vIOMMU-scoped
/// invalidate already covers every stream behind the emulated SMMU, and the
/// host kernel offers no per-device invalidate ioctl for the nested path. So
/// there is exactly one sink per accelerated SMMU, and a guest invalidation is
/// forwarded **once** (not once per device), eliminating the per-device
/// fan-out that would otherwise turn one guest command into M identical host
/// syscalls for M devices.
///
/// The emulator accumulates consecutive forwardable CMDQ commands and flushes
/// them to this sink as one batch (one `IOMMU_HWPT_INVALIDATE`) at each
/// synchronization or configuration boundary, collapsing a shootdown burst of
/// N commands from N syscalls to one.
///
/// Each entry is the raw 128-bit CMDQ command as a little-endian `[qw0, qw1]`
/// quadword pair; the host kernel parses the opcode and operands. Keeping the
/// interface a plain `[u64; 2]` keeps this crate free of any host-IOMMU binding
/// types.
pub trait AcceleratedInvalidationSink: Send + Sync {
    /// Forward `entries` to the host as one ordered batch, preserving program
    /// order.
    ///
    /// Returns `Ok(())` if the host handled the entire batch. Returns
    /// `Err(handled)` if it did not, where `handled` is the number of leading
    /// entries the host accepted — so the command at index `handled` is the
    /// first one that failed, and the emulator stops draining there and raises
    /// a CMDQ error. `handled` is always `< entries.len()` on the `Err` path.
    fn invalidate(&self, entries: &[[u64; 2]]) -> Result<(), usize>;
}

/// A decoded stream (STE) configuration the SMMU emulator dispatches to an
/// [`AcceleratedStreamBackend`].
///
/// The emulator decodes the guest's STE (validity and `STE.Config`) into one
/// of these variants so the backend never has to interpret raw STE bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamConfig {
    /// Abort all transactions. Produced for an invalid STE (`V=0`),
    /// `Config=ABORT`, or any config the emulator does not support in
    /// accelerated mode.
    Abort,
    /// Bypass translation (`Config=BYPASS`) — identity GPA→HPA via the
    /// nesting parent (S2) HWPT.
    Bypass,
    /// Stage-1 translation (`Config=S1_TRANS`). Carries the stream ID (for
    /// lazy vDevice allocation) and the raw stage-1 STE double-words.
    Translate {
        /// Stream ID this configuration applies to. Used by the backend to
        /// allocate the iommufd vDevice (the virtual stream ID is not known
        /// at backend construction time).
        sid: u32,
        /// Canonical stage-1 STE double-words `[DW0, DW1]`: the guest STE
        /// reduced to the fields that are architecturally meaningful under this
        /// vSMMU's advertised capabilities, with every RES0/IGNORED field
        /// zeroed (see [`canonical_s1_ste_dwords`]). A host nesting binding can
        /// consume these as-is; no further masking is required, because the
        /// canonical set is validated against the host at attach time.
        ste_dwords: [u64; 2],
    },
}

/// Reduce a guest stage-1 STE to the double-words that are architecturally
/// meaningful under this vSMMU's advertised capabilities, zeroing every field
/// that is RES0 or IGNORED.
///
/// This is **architectural canonicalization**, not host-ABI masking: which
/// fields survive is a consequence of the SMMUv3 architecture plus the
/// capabilities this emulator advertises, so it belongs to the emulator rather
/// than any host binding. Under the current fixed capabilities the dropped
/// fields are all RES0/IGNORED:
///
/// - `S2P=0` → all stage-2 fields (S2FWB, S2HWU) are RES0;
/// - `ATTR_TYPES_OVR=0` → MTCFG, MemAttr, SHCFG, ALLOCCFG are RES0;
/// - `ATTR_PERMS_OVR=0` → NSCFG, PRIVCFG, INSTCFG are RES0;
/// - `HYP=0` → STRW is fixed to NS-EL1 (0);
/// - unadvertised optional features (S1PIE, S1MPAM, CONT, DCP, DRE, PPAR, MEV)
///   are RES0/IGNORED; the SW bits have no hardware effect.
///
/// Retained — DW0: V, Config, S1Fmt, S1ContextPtr, S1CDMax;
/// DW1: S1DSS, S1CIR, S1COR, S1CSH, S1STALLD, EATS. Rebuilding the words through
/// the typed field setters keeps the selection tied to the spec definitions.
///
/// NOTE: if this emulator ever advertises one of the above features, the
/// retained set must grow to match (and attach-time capability resolution must
/// gate the new field against the host SMMU).
fn canonical_s1_ste_dwords(ste: &crate::spec::ste::Ste) -> [u64; 2] {
    use crate::spec::ste::SteDw0;
    use crate::spec::ste::SteDw1;

    let dw0 = SteDw0::new()
        .with_v(ste.qw0.v())
        .with_config(ste.qw0.config())
        .with_s1_fmt(ste.qw0.s1_fmt())
        .with_s1_context_ptr(ste.qw0.s1_context_ptr())
        .with_s1_cd_max(ste.qw0.s1_cd_max());

    let dw1 = SteDw1::new()
        .with_s1_dss(ste.qw1.s1_dss())
        .with_s1_cir(ste.qw1.s1_cir())
        .with_s1_cor(ste.qw1.s1_cor())
        .with_s1_csh(ste.qw1.s1_csh())
        .with_s1stalld(ste.qw1.s1stalld())
        .with_eats(ste.qw1.eats());

    [dw0.into(), dw1.into()]
}

/// Registration entry for a VFIO device with iommufd-accelerated translation.
///
/// The SID is derived dynamically from the `bus_range` (which holds the
/// guest-assigned bus number) rather than being fixed at registration time,
/// because PCIe bus numbers are assigned by the guest during enumeration.
struct AccelDeviceRegistration {
    /// The device's assigned bus range (shared with the PCIe port).
    bus_range: AssignedBusRange,
    /// Offset into this SMMU's stream table for the device's root complex.
    stream_id_base: u32,
    /// The iommufd-backed stream handler.
    backend: Arc<dyn AcceleratedStreamBackend>,
    /// The stream ID for which this device is currently attached in
    /// translating (`S1_TRANS`) mode on the host, or `None` when the most
    /// recently applied config is bypass/abort (or an apply failed).
    ///
    /// This is the exact vSID the backend used to allocate the host vDevice
    /// and attach the nested HWPT, recorded at apply time rather than
    /// recomputed at query time, so it matches the host attach state exactly.
    /// It gates SID-based invalidation forwarding. Read and written only under
    /// the `accel_devices` lock.
    translating_sid: Option<u32>,
}

/// Composes an SMMU-local stream ID from a bus range, a base offset,
/// and an optional per-device BDF.
///
/// The stream ID is `stream_id_base + (bdf & 0xFFFF)`. When `devid`
/// is `None`, the default BDF `(secondary_bus, dev 0, fn 0)` is used.
///
/// Returns `None` if the secondary bus has not been assigned yet
/// (still 0) or if the BDF's bus number falls outside the port's
/// assigned range.
fn compose_stream_id(
    bus_range: &AssignedBusRange,
    stream_id_base: u32,
    devid: Option<u32>,
) -> Option<u32> {
    let (secondary, subordinate) = bus_range.bus_range();
    if secondary == 0 {
        return None;
    }
    let bdf = devid.unwrap_or((secondary as u32) << 8);
    let bus = (bdf >> 8) as u8;
    if bus < secondary || bus > subordinate {
        tracelimit::warn_ratelimited!(bus, secondary, subordinate, "BDF out of port bus range");
        return None;
    }
    Some(stream_id_base + (bdf & 0xFFFF))
}

/// Result of an SMMU translation attempt.
#[derive(Debug)]
enum TranslateResult {
    /// SMMU disabled (with `GBPA.ABORT=0`) or bus not yet assigned — bypass
    /// (IOVA = GPA).
    Bypass,
    /// Translated GPA.
    Translated(u64),
    /// Global abort: the SMMU is disabled with `GBPA.ABORT=1`. The transaction
    /// is terminated with an abort and **no** event record is generated (there
    /// is no stream context to fault against).
    GlobalAbort,
    /// STE-driven abort with **no** event: `STE.Config[2] == 0` (the `0b000`
    /// encoding, and the reserved `0b0xx` encodings which "behave as `0b000`").
    /// Per the SMMUv3 `STE.Config` table these terminate the transaction
    /// without recording an event — distinct from an illegal or invalid STE,
    /// which faults via [`TranslateResult::Fault`].
    Abort,
    /// Translation fault, or an invalid (`V=0`) / illegal STE — records the
    /// carried event (`C_BAD_STE`, `C_BAD_STREAMID`, or a stage-1 walk fault)
    /// to the EVTQ.
    Fault(EvtEntry),
}

/// Shared SMMU state accessed by per-device translation wrappers.
///
/// The SMMU device updates this state on register writes; per-device wrappers
/// read it during translation. The `RwLock` allows concurrent translations
/// (read path) while register writes (write path) are exclusive.
///
/// Queue and error state is behind a separate `Mutex` so that per-device
/// wrappers can write fault events and signal overflow without going through
/// the emulator.
pub struct SmmuSharedState {
    /// Translation configuration — RwLock for concurrent DMA reads.
    inner: RwLock<SharedStateInner>,
    /// Guest memory for reading page tables and stream table entries.
    guest_memory: GuestMemory,
    /// Event queue and global error state — single mutex covers both
    /// because the EVTQ overflow path needs to update GERROR atomically.
    queue_state: Mutex<QueueErrorState>,
    /// Wired SPI interrupt line for event queue signaling.
    evtq_irq: Option<LineInterrupt>,
    /// Wired SPI interrupt line for global error signaling.
    gerror_irq: Option<LineInterrupt>,
    /// Whether this SMMU is in accelerated mode (iommufd nested).
    ///
    /// When `true`, VFIO cdev devices behind this SMMU use hardware-
    /// accelerated S1 translation. When `false`, all devices use the
    /// software page table walk path.
    accel: bool,
    /// How the advertised OAS is resolved against the host SMMU at
    /// device-attach time (see [`resolve_host_caps`](Self::resolve_host_caps)).
    oas_policy: crate::SmmuOasPolicy,
    /// Per-device accelerated backends (VFIO devices with iommufd nested).
    ///
    /// Devices not in this list use the software page table walk path.
    /// The SID is derived dynamically from each entry's `AssignedBusRange`
    /// because bus numbers are guest-assigned after device construction.
    ///
    /// This `Mutex` also serializes "compute current policy + apply to
    /// backend" for accelerated streams: both device registration
    /// (resolver/manager thread) and CMDQ-driven re-config (vCPU thread) hold
    /// it across the policy computation and the backend ioctls, so the two are
    /// totally ordered and the last-applied stream config always reflects the
    /// newest guest intent. It is never nested inside the translation `inner`
    /// lock (the DMA hot path takes `inner` only) and is not on the DMA path.
    accel_devices: Mutex<Vec<AccelDeviceRegistration>>,
    /// The per-vIOMMU invalidation sink for accelerated mode.
    ///
    /// Set once when the first VFIO device behind this SMMU binds. All devices
    /// behind a single emulated SMMU share one vIOMMU and therefore a single
    /// sink, so invalidations are forwarded once per command rather than once
    /// per device. Unset for emulated-only SMMUs (no host to forward to). A
    /// `OnceLock` because it is written once and then read lock-free from the
    /// CMDQ forwarding path.
    invalidation_sink: OnceLock<Arc<dyn AcceleratedInvalidationSink>>,
}

struct SharedStateInner {
    /// Whether the SMMU is enabled (CR0.SMMUEN).
    enabled: bool,
    /// Mirror of `GBPA.ABORT`, kept in sync on GBPA writes. Selects the
    /// disabled-state policy: when the SMMU is disabled, `true` aborts all
    /// transactions and `false` bypasses (IOVA = GPA). Consulted by both the
    /// non-accel translate path and the accel policy computation.
    gbpa_abort: bool,
    /// Stream table base address.
    strtab_base: u64,
    /// Stream table log2 size (number of entries).
    strtab_log2size: u8,
    /// Advertised output address size in bits. Reflected in IDR5.OAS and
    /// used to derive `oas_mask`.
    oas_bits: u8,
    /// Host SMMU capabilities, once an accelerated VFIO device has bound and
    /// [`SmmuSharedState::resolve_host_caps`] has finalized the host-derived
    /// parameters. `None` until then (and always `None` for non-accel SMMUs).
    /// A second device reporting different host caps is rejected — a single
    /// vSMMU cannot be backed by two physical SMMUs.
    resolved_host_caps: Option<crate::HostSmmuCaps>,
    /// Output address mask: `(1 << oas_bits) - 1`. Computed addresses for
    /// STE/CD/PT fetches are masked with this per SMMUv3 §3.4.
    oas_mask: u64,
}

/// Event queue and global error state.
///
/// A single mutex serializes event writes from concurrent DMA fault
/// paths, GERROR updates from both the emulator and DMA overflow,
/// and interrupt line level changes.
struct QueueErrorState {
    // -- Event queue --
    /// EVTQ base GPA (parsed from EVTQ_BASE register).
    evtq_base_addr: u64,
    /// EVTQ log2 size (clamped to IDR1.EVENTQS).
    evtq_log2size: u8,
    /// Whether the event queue is enabled (CR0.EVENTQEN).
    evtq_enabled: bool,
    /// Whether the EVTQ interrupt is enabled (IRQ_CTRL.EVENTQ_IRQEN).
    evtq_irqen: bool,
    /// Producer index (advanced by the SMMU when writing events).
    evtq_prod: u32,
    /// Consumer index (advanced by the guest via MMIO).
    evtq_cons: u32,

    // -- Global error registers (toggle protocol) --
    /// GERROR register — individual error bits toggled by the SMMU.
    gerror: registers::Gerror,
    /// GERRORN register — written by the guest to acknowledge errors.
    gerrorn: registers::Gerror,
    /// Whether the GERROR interrupt is enabled (IRQ_CTRL.GERROR_IRQEN).
    gerror_irqen: bool,
}

/// Saved portion of [`QueueErrorState`] for state save/restore.
///
/// Only the producer/consumer indices and error toggle registers need
/// saving — the remaining fields (`evtq_base_addr`, `evtq_log2size`,
/// `evtq_enabled`, `evtq_irqen`, `gerror_irqen`) are derived from
/// SMMU register state and re-synced on restore.
pub(crate) struct SavedQueueState {
    pub evtq_prod: u32,
    pub evtq_cons: u32,
    pub gerror: u32,
    pub gerrorn: u32,
}

impl SmmuSharedState {
    /// Creates a new shared state with the SMMU disabled.
    ///
    /// `oas_bits` is the initial output address size in bits (e.g., 40 for a
    /// 40-bit physical address space). Computed addresses for STE/CD/PT
    /// fetches are truncated to this width, matching hardware behavior per
    /// SMMUv3 §3.4. `oas_policy` controls whether the value is finalized
    /// against the host SMMU at device-attach time (see
    /// [`Self::resolve_host_caps`]).
    pub(crate) fn new(
        guest_memory: GuestMemory,
        oas_bits: u8,
        oas_policy: crate::SmmuOasPolicy,
        accel: bool,
        evtq_irq: Option<LineInterrupt>,
        gerror_irq: Option<LineInterrupt>,
    ) -> Arc<Self> {
        let oas_mask = (1u64 << oas_bits) - 1;
        Arc::new(Self {
            inner: RwLock::new(SharedStateInner {
                enabled: false,
                gbpa_abort: false,
                strtab_base: 0,
                strtab_log2size: 0,
                oas_bits,
                resolved_host_caps: None,
                oas_mask,
            }),
            guest_memory,
            queue_state: Mutex::new(QueueErrorState {
                evtq_base_addr: 0,
                evtq_log2size: 0,
                evtq_enabled: false,
                evtq_irqen: false,
                evtq_prod: 0,
                evtq_cons: 0,
                gerror: registers::Gerror::new(),
                gerrorn: registers::Gerror::new(),
                gerror_irqen: false,
            }),
            evtq_irq,
            gerror_irq,
            accel,
            oas_policy,
            accel_devices: Mutex::new(Vec::new()),
            invalidation_sink: OnceLock::new(),
        })
    }

    /// Returns whether this SMMU is in accelerated mode (iommufd nested).
    pub fn is_accel(&self) -> bool {
        self.accel
    }

    /// Returns the currently advertised output address size in bits.
    pub(crate) fn oas_bits(&self) -> u8 {
        self.inner.read().oas_bits
    }

    /// Finalizes the host-derived vSMMU parameters against the physical SMMU
    /// backing an accelerated device, and validates host/guest compatibility.
    ///
    /// Called when an accelerated VFIO device binds to iommufd, at which
    /// point the backing physical SMMU is first known. Runs once per vSMMU:
    /// the first device validates compatibility (TTF, TTENDIAN, GRAN4K) and
    /// applies every host-derived parameter according to its configured
    /// policy (currently OAS — `auto` adopts the host value; `fixed` is
    /// validated as an upper bound). Subsequent devices must report identical
    /// host caps; a mismatch is rejected, since a single vSMMU cannot be
    /// backed by two different physical SMMUs.
    ///
    /// The compatibility checks cover only the features this emulator
    /// actually advertises that the host hardware must honor when walking the
    /// guest's page tables. Features the emulator does not advertise
    /// (SSIDSIZE, ATS, RIL, 16K/64K granules, 2-level stream tables) are
    /// intentionally not checked — see the TODOs at the IDR advertisement in
    /// `emulator.rs`. The host stream-ID size (IDR1.SIDSIZE) and stream-table
    /// format (IDR0.ST_LEVEL) are deliberately *not* validated: in the nested
    /// path the host never indexes or walks the guest's stream table (the VMM
    /// emulates it and registers each guest StreamID individually via
    /// `IOMMU_VDEVICE_ALLOC`), so the host and guest stream-table parameters
    /// are independent.
    pub fn resolve_host_caps(&self, caps: crate::HostSmmuCaps) -> anyhow::Result<()> {
        let mut inner = self.inner.write();

        if let Some(existing) = inner.resolved_host_caps {
            if existing != caps {
                anyhow::bail!(
                    "SMMU already bound to a physical SMMU ({existing:?}), but another \
                     device reports different host capabilities ({caps:?}); a single \
                     vSMMU cannot be backed by two physical SMMUs"
                );
            }
            return Ok(());
        }

        // TTF: the emulator builds AArch64 S1 page tables, so the host must be
        // able to walk them. TTF is a bitfield, not an ordered value — test
        // the AArch64 bit rather than comparing.
        if !caps.ttf.aarch64() {
            anyhow::bail!(
                "host SMMU does not support AArch64 translation tables \
                 (IDR0.TTF={:#05b})",
                u8::from(caps.ttf)
            );
        }

        // TTENDIAN: the emulator uses little-endian table walks. The encoding
        // is a set of distinct configurations, not an ordered range — test
        // membership rather than comparing.
        if !matches!(
            caps.ttendian,
            registers::Idr0TtEndian::MIXED | registers::Idr0TtEndian::LE
        ) {
            anyhow::bail!(
                "host SMMU does not support little-endian translation tables \
                 (IDR0.TTENDIAN={:#04b})",
                caps.ttendian.0
            );
        }

        // GRAN4K: the guest builds 4KB S1 page tables, so the host hardware
        // must support the 4KB granule.
        if !caps.gran4k {
            anyhow::bail!("host SMMU does not support the 4KB translation granule (IDR5.GRAN4K=0)");
        }

        // OAS: decode the host's IDR5.OAS encoding (may be a reserved value),
        // then `auto` adopts the host value while `fixed` must not exceed it.
        let host_oas_bits = caps.oas.bits().ok_or_else(|| {
            anyhow::anyhow!(
                "host SMMU reported an unknown OAS encoding ({})",
                caps.oas.0
            )
        })?;
        match self.oas_policy {
            crate::SmmuOasPolicy::Auto { .. } => {
                inner.oas_bits = host_oas_bits;
                inner.oas_mask = (1u64 << host_oas_bits) - 1;
            }
            crate::SmmuOasPolicy::Fixed(oas) => {
                if oas > host_oas_bits {
                    anyhow::bail!(
                        "configured SMMU oas={oas} exceeds host SMMU OAS {host_oas_bits}; \
                         lower the configured OAS or use oas=auto"
                    );
                }
            }
        }

        inner.resolved_host_caps = Some(caps);
        Ok(())
    }

    /// Updates the SMMU enable state (called by SmmuDevice on CR0 writes) and
    /// atomically re-drives accelerated backends to the new policy.
    ///
    /// The state write and the re-drive happen under a single `accel_devices`
    /// lock acquisition, so the transition is atomic with respect to device
    /// registration and other policy changes: a backend can never observe a
    /// half-updated view and apply a stale policy that then "wins".
    pub(crate) fn set_enabled(&self, enabled: bool) {
        let mut devices = self.accel_devices.lock();
        self.inner.write().enabled = enabled;
        self.apply_all_locked(&mut devices);
    }

    /// Updates the mirrored `GBPA.ABORT` state (called by SmmuDevice on GBPA
    /// writes) and atomically re-drives accelerated backends to the new
    /// policy. Selects the disabled-state policy (abort vs bypass).
    ///
    /// Like [`set_enabled`](Self::set_enabled), the write and the re-drive are
    /// a single `accel_devices` lock critical section.
    pub(crate) fn set_gbpa_abort(&self, abort: bool) {
        let mut devices = self.accel_devices.lock();
        self.inner.write().gbpa_abort = abort;
        self.apply_all_locked(&mut devices);
    }

    /// Atomically replaces all policy-relevant translation state (enable,
    /// `GBPA.ABORT`, stream table base/size) and re-drives accelerated
    /// backends to the resulting policy, in a single `accel_devices` lock
    /// critical section.
    ///
    /// Used on device reset and state restore, where several policy inputs
    /// change together: applying them as one atomic transition (rather than a
    /// sequence of single-field updates) avoids transient intermediate
    /// policies and any ordering fragility around when the final re-drive
    /// observes fully-consistent state.
    pub(crate) fn sync_translation_state(
        &self,
        enabled: bool,
        gbpa_abort: bool,
        strtab_base: u64,
        strtab_log2size: u8,
    ) {
        let mut devices = self.accel_devices.lock();
        {
            let mut inner = self.inner.write();
            inner.enabled = enabled;
            inner.gbpa_abort = gbpa_abort;
            inner.strtab_base = strtab_base;
            inner.strtab_log2size = strtab_log2size;
        }
        self.apply_all_locked(&mut devices);
    }

    /// Updates the stream table configuration (called by SmmuDevice on
    /// STRTAB_BASE / STRTAB_BASE_CFG writes).
    pub(crate) fn set_strtab(&self, base: u64, log2size: u8) {
        let mut inner = self.inner.write();
        inner.strtab_base = base;
        inner.strtab_log2size = log2size;
    }

    /// Updates the event queue configuration (called by SmmuDevice on
    /// EVTQ_BASE writes).
    pub(crate) fn set_evtq_config(&self, base_addr: u64, log2size: u8) {
        let mut qs = self.queue_state.lock();
        qs.evtq_base_addr = base_addr;
        qs.evtq_log2size = log2size;
    }

    /// Updates the event queue enabled state (called on CR0 writes).
    pub(crate) fn set_evtq_enabled(&self, enabled: bool) {
        self.queue_state.lock().evtq_enabled = enabled;
    }

    /// Updates both interrupt enable flags from IRQ_CTRL (called on
    /// IRQ_CTRL writes). Also updates the GERROR interrupt line level.
    pub(crate) fn set_irq_ctrl(&self, evtq_irqen: bool, gerror_irqen: bool) {
        let mut qs = self.queue_state.lock();
        qs.evtq_irqen = evtq_irqen;
        qs.gerror_irqen = gerror_irqen;
        self.update_gerror_irq(&qs);
    }

    /// Reads the current GERROR register value.
    pub(crate) fn read_gerror(&self) -> registers::Gerror {
        self.queue_state.lock().gerror
    }

    /// Reads the current GERRORN register value.
    pub(crate) fn read_gerrorn(&self) -> registers::Gerror {
        self.queue_state.lock().gerrorn
    }

    /// Returns true if GERROR.CMDQ_ERR != GERRORN.CMDQ_ERR (error active).
    pub(crate) fn cmdq_err_active(&self) -> bool {
        let qs = self.queue_state.lock();
        qs.gerror.cmdq_err() != qs.gerrorn.cmdq_err()
    }

    /// Writes GERRORN (guest acknowledging errors) and updates the
    /// interrupt line level.
    pub(crate) fn write_gerrorn(&self, value: u32) {
        let mut qs = self.queue_state.lock();
        qs.gerrorn = registers::Gerror::from(value);
        self.update_gerror_irq(&qs);
    }

    /// Toggles GERROR.CMDQ_ERR to signal a command queue error.
    ///
    /// Updates the interrupt line level under the lock.
    pub(crate) fn toggle_cmdq_err(&self) {
        let mut qs = self.queue_state.lock();
        let new_val = !qs.gerror.cmdq_err();
        qs.gerror.set_cmdq_err(new_val);
        self.update_gerror_irq(&qs);
    }

    /// Signals an EVTQ overflow by making GERROR.EVTQ_ABT_ERR active.
    ///
    /// Per spec, sets the bit to the inverse of GERRORN.EVTQ_ABT_ERR.
    /// If the error is already active this is a no-op (the bit value
    /// doesn't change). Called from `write_event` under the same lock.
    fn signal_evtq_overflow(&self, qs: &mut QueueErrorState) {
        let new_val = !qs.gerrorn.eventq_abt_err();
        qs.gerror.set_eventq_abt_err(new_val);
        self.update_gerror_irq(qs);
    }

    /// Updates the GERROR wired interrupt line level based on current state.
    ///
    /// Must be called with the queue_state lock held. The line is held
    /// high while any error is active (GERROR != GERRORN) and deasserted
    /// when all errors are acknowledged.
    fn update_gerror_irq(&self, qs: &QueueErrorState) {
        if let Some(irq) = &self.gerror_irq {
            let active = qs.gerror_irqen && u32::from(qs.gerror) != u32::from(qs.gerrorn);
            irq.set_level(active);
        }
    }

    /// Updates the event queue consumer index (called when the guest
    /// writes EVENTQ_CONS on page 1).
    ///
    /// Deasserts the EVTQ wired interrupt if the queue is now empty.
    pub(crate) fn set_evtq_cons(&self, cons: u32) {
        let mut qs = self.queue_state.lock();
        qs.evtq_cons = cons;
        // Deassert EVTQ IRQ when the guest has drained all events.
        if qs.evtq_irqen && qs.evtq_prod == qs.evtq_cons {
            if let Some(irq) = &self.evtq_irq {
                irq.set_level(false);
            }
        }
    }

    /// Returns the current event queue producer index (for guest reads
    /// of EVENTQ_PROD on page 1).
    pub(crate) fn evtq_prod(&self) -> u32 {
        self.queue_state.lock().evtq_prod
    }

    /// Returns the current event queue consumer index (for guest reads
    /// of EVENTQ_CONS on page 1).
    pub(crate) fn evtq_cons(&self) -> u32 {
        self.queue_state.lock().evtq_cons
    }

    /// Resets event queue and GERROR state (called on device reset).
    pub(crate) fn reset_queue_state(&self) {
        let mut qs = self.queue_state.lock();
        qs.evtq_base_addr = 0;
        qs.evtq_log2size = 0;
        qs.evtq_enabled = false;
        qs.evtq_irqen = false;
        qs.evtq_prod = 0;
        qs.evtq_cons = 0;
        qs.gerror = registers::Gerror::new();
        qs.gerrorn = registers::Gerror::new();
        qs.gerror_irqen = false;
        self.update_gerror_irq(&qs);
    }

    /// Saves the queue and error state that must be persisted.
    ///
    /// Fields derived from SMMU registers (`evtq_base_addr`, `evtq_log2size`,
    /// `evtq_enabled`, `evtq_irqen`, `gerror_irqen`) are re-synced on
    /// restore and are not included in the saved state.
    pub(crate) fn save_queue_state(&self) -> SavedQueueState {
        let qs = self.queue_state.lock();
        // Exhaustively destructure to get a compile error if a field is added.
        let QueueErrorState {
            evtq_base_addr: _,
            evtq_log2size: _,
            evtq_enabled: _,
            evtq_irqen: _,
            evtq_prod,
            evtq_cons,
            gerror,
            gerrorn,
            gerror_irqen: _,
        } = *qs;
        SavedQueueState {
            evtq_prod,
            evtq_cons,
            gerror: gerror.into(),
            gerrorn: gerrorn.into(),
        }
    }

    /// Restores the queue and error state from a saved snapshot.
    ///
    /// The caller must re-sync derived fields (`set_evtq_config`,
    /// `set_evtq_enabled`, `set_irq_ctrl`) before this call, since
    /// this function uses `evtq_irqen` to sync the EVTQ interrupt line.
    pub(crate) fn restore_queue_state(&self, state: SavedQueueState) {
        let SavedQueueState {
            evtq_prod,
            evtq_cons,
            gerror,
            gerrorn,
        } = state;
        let mut qs = self.queue_state.lock();
        qs.evtq_prod = evtq_prod;
        qs.evtq_cons = evtq_cons;
        qs.gerror = registers::Gerror::from(gerror);
        qs.gerrorn = registers::Gerror::from(gerrorn);
        self.update_gerror_irq(&qs);
        // Sync EVTQ wired interrupt line to match restored queue state.
        if qs.evtq_irqen {
            if let Some(irq) = &self.evtq_irq {
                irq.set_level(qs.evtq_prod != qs.evtq_cons);
            }
        }
    }

    /// Register an accelerated backend for a VFIO device.
    ///
    /// The device's stream ID is derived dynamically from `bus_range`
    /// (which holds the guest-assigned bus number) rather than being
    /// fixed at registration time. When the guest writes `CFGI_STE` or
    /// TLBI commands, the emulator matches the command's SID against
    /// each registered device's current bus assignment.
    ///
    /// Registration is atomic with applying the SMMU's *current* policy to the
    /// new device (under the `accel_devices` lock), so a freshly attached device lands
    /// in the correct boot state instead of staying fail-closed (detached).
    /// At boot the SMMU is disabled, so the policy is bypass-or-abort per
    /// `GBPA.ABORT` and is independent of the StreamID — it is applied even
    /// before the guest has assigned this device's bus number. Once the SMMU
    /// is enabled the policy depends on the per-stream STE; if the bus is not
    /// yet assigned the device is left fail-closed until the guest enumerates
    /// and issues `CFGI_STE`.
    pub fn register_accel_device(
        &self,
        bus_range: AssignedBusRange,
        stream_id_base: u32,
        backend: Arc<dyn AcceleratedStreamBackend>,
    ) {
        let mut devices = self.accel_devices.lock();
        let mut reg = AccelDeviceRegistration {
            bus_range: bus_range.clone(),
            stream_id_base,
            backend,
            translating_sid: None,
        };

        // Catch the new device up to the current policy.
        //
        // If the bus is assigned, compute the stream-specific policy. If not,
        // fall back to the disabled-state (StreamID-independent) policy — this
        // is what lets a boot device reach bypass/abort before the guest has
        // enumerated it. With the SMMU enabled and no bus yet, there is no
        // policy to apply: leave the device fail-closed (non-translating)
        // until its `CFGI_STE`.
        let config = match compose_stream_id(&bus_range, stream_id_base, None) {
            Some(sid) => Some(self.current_stream_config(sid)),
            None => self.disabled_policy(),
        };
        if let Some(config) = config {
            Self::apply_config(&mut reg, config);
        }
        devices.push(reg);
    }

    /// Computes the SMMU's current policy for the given stream.
    ///
    /// **Pure**: it snapshots register state and reads/decodes the STE, but
    /// records no events. Faults are a *data-plane* concern and are never
    /// synthesized on this config-plane path:
    ///
    /// - For emulated devices, an illegal/invalid STE faults per transaction in
    ///   the software translate path
    ///   ([`translate_locked`](Self::translate_locked)).
    /// - For accelerated (passthrough) devices, the physical SMMU generates the
    ///   fault on the real transaction and the host forwards it via the iommufd
    ///   virtual event queue (VEVENTQ); this emulator does not fake it here.
    ///
    /// An illegal/invalid STE, an out-of-range SID, or an STE fetch failure all
    /// resolve to [`StreamConfig::Abort`] (block the stream's DMA). When the
    /// SMMU is disabled the result is `GBPA.ABORT ? Abort : Bypass`.
    ///
    /// The translation (`inner`) lock is only held to snapshot register state;
    /// it is released before the STE read so callers can apply the result to a
    /// backend (a blocking ioctl) without nesting the translation lock around
    /// it.
    pub(crate) fn current_stream_config(&self, sid: u32) -> StreamConfig {
        let (enabled, gbpa_abort, strtab_base, strtab_log2size, oas_mask) = {
            let inner = self.inner.read();
            (
                inner.enabled,
                inner.gbpa_abort,
                inner.strtab_base,
                inner.strtab_log2size,
                inner.oas_mask,
            )
        };

        if !enabled {
            return if gbpa_abort {
                StreamConfig::Abort
            } else {
                StreamConfig::Bypass
            };
        }

        // SMMU enabled: look up and decode this stream's STE with the same
        // classification (`lookup_ste` + `ste_config_action`) as the software
        // translation path, so the two cannot diverge. Every non-translating,
        // non-bypass outcome blocks the stream's DMA by aborting; the matching
        // fault event, when one is architecturally due, is delivered on the
        // data plane (see the method doc), not here.
        let Ok(ste) = translate::lookup_ste(
            &self.guest_memory,
            strtab_base,
            strtab_log2size,
            sid,
            oas_mask,
        ) else {
            // Invalid STE (V=0), out-of-range SID, or STE fetch failure.
            return StreamConfig::Abort;
        };

        match translate::ste_config_action(&ste) {
            translate::SteAction::Bypass => StreamConfig::Bypass,
            translate::SteAction::S1Translate => StreamConfig::Translate {
                sid,
                ste_dwords: canonical_s1_ste_dwords(&ste),
            },
            // Config[2]==0 (0b000 / reserved) aborts with no event; an illegal
            // config (0b110/0b111 on this stage-1-only SMMU) also aborts here —
            // its C_BAD_STE, being a data-plane fault, is delivered elsewhere.
            translate::SteAction::Abort | translate::SteAction::Illegal => StreamConfig::Abort,
        }
    }

    /// Returns the StreamID-independent policy that applies while the SMMU is
    /// disabled (`Some(Bypass)` or `Some(Abort)` per `GBPA.ABORT`), or `None`
    /// when the SMMU is enabled (the policy then depends on the per-stream
    /// STE).
    fn disabled_policy(&self) -> Option<StreamConfig> {
        let inner = self.inner.read();
        (!inner.enabled).then(|| {
            if inner.gbpa_abort {
                StreamConfig::Abort
            } else {
                StreamConfig::Bypass
            }
        })
    }

    /// Applies `config` to a registered device's backend and records the
    /// stream ID for which the device is now translating (`S1_TRANS`), or
    /// `None` for bypass/abort.
    ///
    /// The recorded `translating_sid` gates SID-based invalidation forwarding
    /// (`CFGI_CD`/`CFGI_CD_ALL`, and `ATC_INV` once ATS is enabled): those
    /// commands target host state — the context-descriptor cache, and the
    /// device's ATS cache — that exists only while the device is attached to a
    /// nested translating domain. It mirrors that attach state (the vDevice is
    /// allocated and the device attached to its nested HWPT on `S1_TRANS`;
    /// both torn down on bypass/abort) and stores the exact vSID the backend
    /// used, so no recomputation is needed at invalidation time. A failed
    /// apply leaves the host state uncertain, so it is cleared to `None`: fail
    /// closed and do not forward invalidations the host could not resolve.
    fn apply_config(reg: &mut AccelDeviceRegistration, config: StreamConfig) {
        let translating_sid = match config {
            StreamConfig::Translate { sid, .. } => Some(sid),
            StreamConfig::Bypass | StreamConfig::Abort => None,
        };
        match reg.backend.set_stream_config(config) {
            Ok(()) => reg.translating_sid = translating_sid,
            Err(e) => {
                reg.translating_sid = None;
                tracelimit::warn_ratelimited!(
                    error = &*e as &dyn std::error::Error,
                    "smmu: failed to apply stream config"
                );
            }
        }
    }

    /// Re-computes and applies the current policy for a single stream's
    /// accelerated backend (if one is registered).
    ///
    /// Serialized against registration and other policy updates via the
    /// policy lock so the last write wins. Used for `CFGI_STE`.
    pub(crate) fn apply_stream_config(&self, sid: u32) {
        let mut devices = self.accel_devices.lock();
        let Some(reg) = devices
            .iter_mut()
            .find(|reg| compose_stream_id(&reg.bus_range, reg.stream_id_base, None) == Some(sid))
        else {
            return;
        };
        let config = self.current_stream_config(sid);
        Self::apply_config(reg, config);
    }

    /// Re-computes and applies the current policy for every registered
    /// accelerated backend.
    ///
    /// Used on events that change policy globally without otherwise mutating
    /// translation state: `CFGI_STE_RANGE` / `CFGI_ALL`. (The state-mutating
    /// events — CR0/GBPA writes, reset, restore — re-drive atomically via
    /// [`set_enabled`](Self::set_enabled),
    /// [`set_gbpa_abort`](Self::set_gbpa_abort), and
    /// [`sync_translation_state`](Self::sync_translation_state).)
    /// Serialized via the policy lock.
    pub(crate) fn apply_all_stream_configs(&self) {
        let mut devices = self.accel_devices.lock();
        self.apply_all_locked(&mut devices);
    }

    /// Re-drives every registered backend to its current policy. The caller
    /// must already hold the `accel_devices` lock and pass in the guarded
    /// slice (this is the shared body of
    /// [`apply_all_stream_configs`](Self::apply_all_stream_configs) and the
    /// state-mutating setters).
    fn apply_all_locked(&self, devices: &mut [AccelDeviceRegistration]) {
        for reg in devices.iter_mut() {
            let Some(sid) = compose_stream_id(&reg.bus_range, reg.stream_id_base, None) else {
                continue;
            };
            let config = self.current_stream_config(sid);
            Self::apply_config(reg, config);
        }
    }

    /// Whether a SID-based invalidation (`CFGI_CD`/`CFGI_CD_ALL`/`ATC_INV`)
    /// targeting `sid` should be forwarded to the host vIOMMU.
    ///
    /// True only when a registered accelerated device is currently attached in
    /// translating (`S1_TRANS`) mode for exactly `sid` — i.e. its recorded
    /// `translating_sid` matches. That recorded value is the vSID the backend
    /// used to allocate the host vDevice and attach the nested HWPT, so the
    /// check reflects the *applied* host attach state — not the guest's STE
    /// bytes, which can run ahead of what has been applied — and stays in
    /// lockstep with the host vDevice / nested-HWPT lifetime.
    ///
    /// These commands target state that exists on the host only while the
    /// stream translates: the context-descriptor cache (`CFGI_CD`), and the
    /// device's ATS cache (`ATC_INV`, which in the nested path is enabled only
    /// for `S1_TRANS` streams). In bypass/abort there is nothing to invalidate
    /// and no vDevice bound, so forwarding would hit `-EIO`. Because the guest
    /// writes the context descriptor (issuing `CFGI_CD`) before installing the
    /// translating STE (`CFGI_STE`) when attaching a device, this also
    /// correctly skips that first premature `CFGI_CD`.
    pub(crate) fn sid_invalidation_forwardable(&self, sid: u32) -> bool {
        self.accel_devices
            .lock()
            .iter()
            .any(|reg| reg.translating_sid == Some(sid))
    }

    /// Registers the per-vIOMMU invalidation sink for accelerated mode.
    ///
    /// Called once per emulated SMMU when the first VFIO device behind it
    /// binds. All devices behind a single emulated SMMU share one vIOMMU and
    /// therefore one sink, so registrations from additional devices are
    /// ignored (the first sink stays in place).
    pub fn register_invalidation_sink(&self, sink: Arc<dyn AcceleratedInvalidationSink>) {
        // First sink wins; additional devices behind the same vIOMMU share it.
        let _ = self.invalidation_sink.set(sink);
    }

    /// Returns the registered invalidation sink, if any.
    ///
    /// Used by CMDQ processing to forward a batch of invalidation commands to
    /// the host. `None` for emulated-only SMMUs.
    pub(crate) fn invalidation_sink(&self) -> Option<Arc<dyn AcceleratedInvalidationSink>> {
        self.invalidation_sink.get().cloned()
    }

    /// Translate an IOVA to a GPA for the given stream ID.
    ///
    /// Callers that need to hold the lock across translation and a subsequent
    /// memory access should use [`translate_with`] instead.
    fn translate(&self, sid: u32, iova: u64, write: bool) -> TranslateResult {
        let inner = self.inner.read();
        self.translate_locked(&inner, sid, iova, write)
    }

    /// Translate an IOVA to a GPA while holding the read lock.
    ///
    /// The caller holds `inner` across both translation and the subsequent
    /// memory access, preventing SMMU config changes (disable, stream table
    /// base update) from creating a TOCTOU between translation and access.
    fn translate_locked(
        &self,
        inner: &SharedStateInner,
        sid: u32,
        iova: u64,
        write: bool,
    ) -> TranslateResult {
        if !inner.enabled {
            // The SMMU is disabled: GBPA selects the global policy. ABORT
            // terminates the transaction (with no event — there is no stream
            // context to fault against); otherwise transactions bypass
            // (IOVA = GPA). The matching accel policy is computed in
            // [`current_stream_config`].
            if inner.gbpa_abort {
                return TranslateResult::GlobalAbort;
            }
            return TranslateResult::Bypass;
        }

        // Look up the STE.
        let ste = match translate::lookup_ste(
            &self.guest_memory,
            inner.strtab_base,
            inner.strtab_log2size,
            sid,
            inner.oas_mask,
        ) {
            Ok(ste) => ste,
            Err(fault) => return TranslateResult::Fault(fault.event),
        };

        // Dispatch on STE config.
        match translate::ste_config_action(&ste) {
            // Config[2]==0 (0b000 / reserved): abort, no event recorded.
            translate::SteAction::Abort => TranslateResult::Abort,
            // Illegal on this stage-1-only SMMU (0b110/0b111): terminate and
            // record C_BAD_STE, matching the spec's "behaves as V=0" rule.
            translate::SteAction::Illegal => TranslateResult::Fault(EvtEntry::bad_ste(sid)),
            translate::SteAction::Bypass => TranslateResult::Bypass,
            translate::SteAction::S1Translate => {
                // Look up the CD.
                let cd =
                    match translate::lookup_cd(&self.guest_memory, &ste, sid, 0, inner.oas_mask) {
                        Ok(cd) => cd,
                        Err(fault) => return TranslateResult::Fault(fault.event),
                    };

                // Extract translation context (caps CD.IPS to device OAS).
                let ctx = match translate::translation_context(&cd, sid, inner.oas_mask) {
                    Ok(ctx) => ctx,
                    Err(fault) => return TranslateResult::Fault(fault.event),
                };

                // Walk the page table.
                match translate::walk_s1(&self.guest_memory, &ctx, iova, write, sid) {
                    Ok(tr) => TranslateResult::Translated(tr.gpa),
                    Err(fault) => TranslateResult::Fault(fault.event),
                }
            }
        }
    }

    /// Write an event record directly to the guest's event queue.
    ///
    /// Called from per-device DMA fault paths and from the emulator's
    /// command processing. If the queue is full, drops the event and
    /// logs a warning. If an event is successfully written, pulses
    /// the EVTQ wired SPI interrupt (if enabled).
    pub(crate) fn write_event(&self, event: EvtEntry) {
        let mut qs = self.queue_state.lock();
        if !qs.evtq_enabled {
            return;
        }

        let max_entries = 1u32 << qs.evtq_log2size;
        let index_mask = (max_entries << 1) - 1;
        let prod = qs.evtq_prod & index_mask;
        let cons = qs.evtq_cons & index_mask;

        // Check if the queue is full. Full when the index bits match but
        // the wrap bit differs: (prod ^ cons) == max_entries.
        if (prod ^ cons) == max_entries {
            // Signal EVTQ overflow via GERROR.EVTQ_ABT_ERR — updates
            // the GERROR register and interrupt line under the same lock.
            self.signal_evtq_overflow(&mut qs);
            tracelimit::warn_ratelimited!("smmu: EVTQ full, dropping event");
            return;
        }

        // Write the 32-byte event record to guest memory.
        let index = prod & (max_entries - 1);
        let entry_addr = qs.evtq_base_addr + (index as u64) * (EvtEntry::SIZE as u64);

        if let Err(e) = self.guest_memory.write_at(entry_addr, event.as_bytes()) {
            tracelimit::warn_ratelimited!(
                error = &e as &dyn std::error::Error,
                entry_addr,
                "smmu: failed to write EVTQ entry to guest memory"
            );
            return;
        }

        // Advance EVTQ_PROD.
        qs.evtq_prod = (prod + 1) & index_mask;

        // Assert EVTQ wired interrupt — held high while queue is non-empty.
        // Deasserted when the guest drains events via CONS writes.
        if qs.evtq_irqen {
            if let Some(irq) = &self.evtq_irq {
                irq.set_level(true);
            }
        }
    }

    /// Creates a translator for PCI devices behind this SMMU.
    ///
    /// `stream_id_base` is the offset into this SMMU's stream table for the
    /// root complex this device belongs to. The translator computes the
    /// stream ID as `stream_id_base + rid` at each access.
    pub fn translator(self: &Arc<Self>, stream_id_base: u32) -> SmmuTranslator {
        SmmuTranslator {
            shared: self.clone(),
            stream_id_base,
        }
    }

    /// Creates an SMMU irqfd wrapper for a PCI device behind this SMMU.
    ///
    /// `stream_id_base` is the offset into this SMMU's stream table for the
    /// root complex this device belongs to.
    ///
    /// Irqfd routes created from the returned wrapper will translate MSI
    /// addresses through the SMMU page tables before programming the
    /// kernel route.
    pub fn wrap_irqfd(
        self: &Arc<Self>,
        stream_id_base: u32,
        inner: Arc<dyn IrqFd>,
    ) -> Arc<SmmuIrqFd> {
        Arc::new(SmmuIrqFd {
            shared: self.clone(),
            stream_id_base,
            inner,
        })
    }
}

/// An [`IommuTranslator`](iommu_common::IommuTranslator) for the ARM SMMUv3.
///
/// One `SmmuTranslator` is shared by all PCI devices behind the same SMMU.
/// The requester ID (RID / BDF) is passed at each translation call and
/// combined with the `stream_id_base` to form the SMMU stream ID.
#[derive(Clone)]
pub struct SmmuTranslator {
    shared: Arc<SmmuSharedState>,
    /// Offset into the SMMU's stream table for this root complex.
    stream_id_base: u32,
}

/// DMA translation error from the SMMU.
///
/// The fault event has already been queued to the SMMU's event queue;
/// this error carries the key fields for diagnostic purposes.
#[derive(Debug, thiserror::Error)]
#[error("SMMU DMA fault: event {event_id:#04x} SID {sid:#x} addr {input_addr:#x}")]
pub struct SmmuDmaFault {
    /// Event type ID.
    event_id: u8,
    /// StreamID of the faulting device.
    sid: u32,
    /// Faulting input address.
    input_addr: u64,
}

impl SmmuDmaFault {
    fn from_event(event: &EvtEntry) -> Self {
        Self {
            event_id: event.header.event_id(),
            sid: event.sid,
            input_addr: event.input_addr,
        }
    }

    /// A termination with **no** event record generated — either a global
    /// abort (disabled SMMU, `GBPA.ABORT=1`) or an STE-driven abort
    /// (`STE.Config[2]==0`). `event_id` is 0 to signify "no event".
    fn no_event_abort(sid: u32, input_addr: u64) -> Self {
        Self {
            event_id: 0,
            sid,
            input_addr,
        }
    }
}

impl iommu_common::IommuTranslator for SmmuTranslator {
    type Error = SmmuDmaFault;

    fn max_iova(&self) -> u64 {
        // The SMMUv3 architecture supports up to 48-bit input addresses.
        // This is the maximum across all configurations: stage-1 only,
        // stage-2 only, and nested (stage-1 IAS and stage-2 IPA width
        // are both bounded by 48 bits).
        1u64 << 48
    }

    fn translate<R>(
        &self,
        rid: u16,
        iova: u64,
        write: bool,
        op: impl FnOnce(u64) -> R,
    ) -> Result<R, iommu_common::TranslationFault<SmmuDmaFault>> {
        let sid = self.stream_id_base + (rid as u32);

        // Hold the read lock across translate + op to prevent SMMU config
        // from changing between getting the GPA and using it.
        let inner = self.shared.inner.read();
        let gpa = match self.shared.translate_locked(&inner, sid, iova, write) {
            TranslateResult::Bypass => iova,
            TranslateResult::Translated(gpa) => gpa,
            TranslateResult::GlobalAbort | TranslateResult::Abort => {
                drop(inner);
                // Terminate with no event recorded: either a disabled SMMU
                // (`GBPA.ABORT=1`), or a valid STE whose `Config[2]==0`
                // (`0b000` / reserved) aborts without a fault event.
                return Err(iommu_common::TranslationFault {
                    iova,
                    error: SmmuDmaFault::no_event_abort(sid, iova),
                });
            }
            TranslateResult::Fault(event) => {
                drop(inner);
                let error = SmmuDmaFault::from_event(&event);
                self.shared.write_event(event);
                return Err(iommu_common::TranslationFault { iova, error });
            }
        };

        let result = op(gpa);
        drop(inner);
        Ok(result)
    }
}

/// A [`SignalMsi`] wrapper that translates MSI addresses through the SMMU.
///
/// When a device behind the SMMU fires an MSI, the MSI address may be an
/// IOVA (Linux maps MSI doorbell pages into the device's IOVA space via
/// `iommu_dma_prepare_msi()`). This wrapper translates the address before
/// forwarding to the inner MSI target (typically an ITS or GICv2m wrapper).
pub struct SmmuSignalMsi {
    shared: Arc<SmmuSharedState>,
    /// Offset into the SMMU's stream table for this root complex.
    stream_id_base: u32,
    inner: Arc<dyn SignalMsi>,
}

impl SmmuSignalMsi {
    /// Creates a new SMMU MSI translator wrapping the given inner target.
    pub fn new(
        shared: Arc<SmmuSharedState>,
        stream_id_base: u32,
        inner: Arc<dyn SignalMsi>,
    ) -> Self {
        Self {
            shared,
            stream_id_base,
            inner,
        }
    }
}

impl SignalMsi for SmmuSignalMsi {
    fn signal_msi(&self, devid: Option<u32>, address: u64, data: u32) {
        // MsiTarget resolves devid to a BDF before calling us.
        let Some(bdf) = devid else {
            return;
        };
        let sid = self.stream_id_base + (bdf & 0xFFFF);

        match self.shared.translate(sid, address, true) {
            TranslateResult::Bypass => {
                self.inner.signal_msi(devid, address, data);
            }
            TranslateResult::Translated(gpa) => {
                self.inner.signal_msi(devid, gpa, data);
            }
            TranslateResult::GlobalAbort | TranslateResult::Abort => {
                // No event recorded: disabled SMMU (`GBPA.ABORT=1`) or an
                // STE with `Config[2]==0`. Drop the MSI.
                tracelimit::warn_ratelimited!(sid, address, "smmu: MSI aborted, no event");
            }
            TranslateResult::Fault(event) => {
                self.shared.write_event(event);
                tracelimit::warn_ratelimited!(sid, address, "smmu: MSI translation fault");
            }
        }
    }
}

/// An [`IrqFd`] wrapper that produces SMMU-translating irqfd routes.
///
/// When a device behind the SMMU programs its MSI-X table, the MSI address
/// may be an IOVA. This wrapper creates [`SmmuIrqFdRoute`] instances that
/// translate the address through the SMMU before forwarding to the inner
/// irqfd route (which may itself be an ITS wrapper).
pub struct SmmuIrqFd {
    shared: Arc<SmmuSharedState>,
    /// Offset into the SMMU's stream table for this root complex.
    stream_id_base: u32,
    inner: Arc<dyn IrqFd>,
}

impl IrqFd for SmmuIrqFd {
    fn new_irqfd_route(&self) -> anyhow::Result<Box<dyn IrqFdRoute>> {
        let inner_route = self.inner.new_irqfd_route()?;
        Ok(Box::new(SmmuIrqFdRoute {
            shared: self.shared.clone(),
            stream_id_base: self.stream_id_base,
            inner: inner_route,
        }))
    }
}

/// An [`IrqFdRoute`] wrapper that translates the MSI address through the
/// SMMU on [`enable`](IrqFdRoute::enable).
///
/// Translation happens at route-programming time (when the guest writes
/// the MSI-X table), not per-interrupt. If the guest changes SMMU page
/// tables after programming MSI-X, it must also re-program the MSI-X
/// entry (which is the normal flow — the IOMMU driver does this).
struct SmmuIrqFdRoute {
    shared: Arc<SmmuSharedState>,
    /// Offset into the SMMU's stream table for this root complex.
    stream_id_base: u32,
    inner: Box<dyn IrqFdRoute>,
}

impl IrqFdRoute for SmmuIrqFdRoute {
    fn event(&self) -> &Event {
        self.inner.event()
    }

    fn enable(&self, address: u64, data: u32, devid: Option<u32>) {
        // MsiRoute resolves devid to a BDF before calling us.
        let Some(bdf) = devid else {
            return;
        };
        let sid = self.stream_id_base + (bdf & 0xFFFF);

        match self.shared.translate(sid, address, true) {
            TranslateResult::Bypass => {
                self.inner.enable(address, data, devid);
            }
            TranslateResult::Translated(gpa) => {
                self.inner.enable(gpa, data, devid);
            }
            TranslateResult::GlobalAbort | TranslateResult::Abort => {
                // No event recorded: disabled SMMU (`GBPA.ABORT=1`) or an
                // STE with `Config[2]==0`. Drop the route.
                tracelimit::warn_ratelimited!(
                    sid,
                    address,
                    "smmu: irqfd MSI route aborted, no event"
                );
            }
            TranslateResult::Fault(event) => {
                self.shared.write_event(event);
                tracelimit::warn_ratelimited!(
                    sid,
                    address,
                    "smmu: irqfd MSI route translation fault"
                );
            }
        }
    }

    fn disable(&self) {
        self.inner.disable();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::cd::CD_SIZE;
    use crate::spec::cd::CdDw0;
    use crate::spec::cd::CdDw1;
    use crate::spec::cd::Ips;
    use crate::spec::cd::Tg0;
    use crate::spec::events::EventId;
    use crate::spec::pt::ApBits;
    use crate::spec::pt::PtDesc;
    use crate::spec::ste::S1Fmt;
    use crate::spec::ste::STE_SIZE;
    use crate::spec::ste::Ste;
    use crate::spec::ste::SteConfig;
    use crate::spec::ste::SteDw0;
    use crate::spec::ste::SteDw1;
    use parking_lot::Mutex;
    use pci_core::bus_range::AssignedBusRange;
    use std::sync::Arc;

    // Memory layout for tests. All addresses fit within a 6 MB allocation
    // to avoid excessive memory usage in test processes.
    const STRTAB_BASE: u64 = 0x10_0000;
    const STRTAB_LOG2SIZE: u8 = 10;
    const CD_BASE: u64 = 0x20_0000;
    const PT_L1_BASE: u64 = 0x30_1000;
    const PT_L2_BASE: u64 = 0x30_2000;
    const PT_L3_BASE: u64 = 0x30_3000;
    // DATA_GPA and EVTQ_BASE are kept low so the guest memory allocation
    // does not need to span gigabytes. Tests read/write data at DATA_GPA
    // and the SMMU writes fault events at EVTQ_BASE.
    const DATA_GPA: u64 = 0x40_0000;
    /// EVTQ base GPA for tests (must not overlap other test regions).
    const EVTQ_BASE: u64 = 0x50_0000;
    /// EVTQ log2 size for tests (3 = 8 entries).
    const EVTQ_LOG2SIZE: u8 = 3;
    const TEST_SEGMENT: u16 = 0;
    /// Stream ID base for the test root complex (matches IORT output_base).
    const TEST_STREAM_ID_BASE: u32 = (TEST_SEGMENT as u32) << 16;
    const TEST_BUS: u8 = 1;
    /// The RID for the test device: (bus << 8) | devfn.
    const TEST_RID: u32 = (TEST_BUS as u32) << 8;

    #[test]
    fn test_canonical_s1_ste_dwords_preserves_allowed_fields() {
        // Set every field the canonical set retains, with distinct values.
        let cd_addr: u64 = 0x3_FFFF_FFFF_F000;
        let qw0 = SteDw0::new()
            .with_v(true)
            .with_config(SteConfig::S1_TRANS.0)
            .with_s1_fmt(S1Fmt::TWO_LEVEL_64K.0)
            .with_s1_context_ptr(cd_addr >> 6)
            .with_s1_cd_max(0x1f);
        let qw1 = SteDw1::new()
            .with_s1_dss(0x3)
            .with_s1_cir(0x3)
            .with_s1_cor(0x3)
            .with_s1_csh(0x3)
            .with_s1stalld(true)
            .with_eats(0x3);
        let ste = Ste {
            qw0,
            qw1,
            _qw2_7: [0; 6],
        };

        let [out0, out1] = canonical_s1_ste_dwords(&ste);
        // Retained fields survive untouched.
        assert_eq!(out0, u64::from(qw0));
        assert_eq!(out1, u64::from(qw1));
    }

    #[test]
    fn test_canonical_s1_ste_dwords_drops_res0_fields() {
        // A fully-populated STE must be reduced to only the retained fields.
        let ste = Ste {
            qw0: SteDw0::from(u64::MAX),
            qw1: SteDw1::from(u64::MAX),
            _qw2_7: [u64::MAX; 6],
        };
        let [out0, out1] = canonical_s1_ste_dwords(&ste);

        // DW0 retained: V[0] | Config[3:1] | S1Fmt[5:4] | S1ContextPtr[55:6] |
        // S1CDMax[63:59]. The reserved bits [58:56] between S1ContextPtr and
        // S1CDMax must be cleared (a real SMMU ignores them; the Linux nesting
        // path rejects them with -EIO).
        assert_eq!(out0, 0xf8ff_ffff_ffff_ffff);
        // DW1 retained: S1DSS[1:0] | S1CIR[3:2] | S1COR[5:4] | S1CSH[7:6] |
        // S1STALLD[27] | EATS[29:28]. Everything else (STRW, SHCFG, NSCFG,
        // PRIVCFG, stage-2/override fields, ...) is RES0/IGNORED and cleared.
        assert_eq!(out1, 0x3800_00ff);
    }

    /// A mock SignalMsi that records calls.
    struct MockSignalMsi {
        calls: Mutex<Vec<(Option<u32>, u64, u32)>>,
    }

    impl MockSignalMsi {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
            })
        }

        fn take_calls(&self) -> Vec<(Option<u32>, u64, u32)> {
            std::mem::take(&mut *self.calls.lock())
        }
    }

    impl SignalMsi for MockSignalMsi {
        fn signal_msi(&self, devid: Option<u32>, address: u64, data: u32) {
            self.calls.lock().push((devid, address, data));
        }
    }

    fn make_bus_range() -> AssignedBusRange {
        let br = AssignedBusRange::new();
        br.set_bus_range(TEST_BUS, TEST_BUS);
        br
    }

    fn expected_sid() -> u32 {
        TEST_STREAM_ID_BASE + ((TEST_BUS as u32) << 8)
    }

    /// Test-only helper: creates a translating GuestMemory and SmmuSignalMsi
    /// pair for a device behind the SMMU.
    fn device_context(
        state: &Arc<SmmuSharedState>,
        bus_range: AssignedBusRange,
        stream_id_base: u32,
        inner_gm: &GuestMemory,
        inner_msi: Arc<dyn SignalMsi>,
    ) -> (GuestMemory, Arc<SmmuSignalMsi>) {
        let translator = state.translator(stream_id_base);
        let gm = iommu_common::TranslatingMemory::new_guest_memory(
            "smmu-translating",
            translator,
            bus_range,
            inner_gm.clone(),
        );
        let signal_msi = Arc::new(SmmuSignalMsi::new(state.clone(), stream_id_base, inner_msi));
        (gm, signal_msi)
    }

    fn write_ste(gm: &GuestMemory, sid: u32, ste: &Ste) {
        let addr = STRTAB_BASE + (sid as u64) * (STE_SIZE as u64);
        gm.write_plain(addr, ste).expect("write STE");
    }

    fn make_s1_ste(cd_base: u64) -> Ste {
        use crate::spec::cd::CD_SIZE;
        let _ = CD_SIZE;
        Ste {
            qw0: SteDw0::new()
                .with_v(true)
                .with_config(SteConfig::S1_TRANS.0)
                .with_s1_context_ptr(cd_base >> 6)
                .with_s1_cd_max(0),
            qw1: SteDw1::new(),
            _qw2_7: [0; 6],
        }
    }

    fn make_bypass_ste() -> Ste {
        Ste {
            qw0: SteDw0::new().with_v(true).with_config(SteConfig::BYPASS.0),
            qw1: SteDw1::new(),
            _qw2_7: [0; 6],
        }
    }

    fn make_abort_ste() -> Ste {
        Ste {
            qw0: SteDw0::new().with_v(true).with_config(SteConfig::ABORT.0),
            qw1: SteDw1::new(),
            _qw2_7: [0; 6],
        }
    }

    fn write_cd(gm: &GuestMemory, cd_base: u64, ssid: u32) {
        use crate::spec::cd::Cd;
        let cd = Cd {
            qw0: CdDw0::new()
                .with_v(true)
                .with_t0sz(32)
                .with_tg0(Tg0::GRAN_4K.0)
                .with_ips(Ips::IPS_40.0)
                .with_aa64(true)
                .with_a(true)
                .with_asid(1),
            qw1: CdDw1::new().with_ttb0(PT_L1_BASE >> 4),
            _qw2: 0,
            mair0: 0xFF440C0400,
            mair1: 0,
            _qw5_7: [0; 3],
        };
        let addr = cd_base + (ssid as u64) * (CD_SIZE as u64);
        gm.write_plain(addr, &cd).expect("write CD");
    }

    fn table_desc(next_table: u64) -> u64 {
        PtDesc::new()
            .with_valid(true)
            .with_desc_type(true)
            .with_addr_bits(next_table >> 12)
            .into()
    }

    fn page_desc(output_addr: u64) -> u64 {
        PtDesc::new()
            .with_valid(true)
            .with_desc_type(true)
            .with_af(true)
            .with_ap(ApBits::RW_EL1.0)
            .with_addr_bits(output_addr >> 12)
            .into()
    }

    fn write_pt_desc(gm: &GuestMemory, addr: u64, desc: u64) {
        gm.write_plain(addr, &desc).expect("write PT desc");
    }

    /// Set up a complete SMMU translation context:
    /// STE (S1_TRANS) → CD → page table mapping IOVA 0..4K → DATA_GPA.
    fn setup_translation(gm: &GuestMemory, sid: u32) {
        // Write STE.
        write_ste(gm, sid, &make_s1_ste(CD_BASE));
        // Write CD.
        write_cd(gm, CD_BASE, 0);
        // Build 3-level page table (T0SZ=32, 4K granule: L1, L2, L3).
        // L1[0] → L2
        write_pt_desc(gm, PT_L1_BASE, table_desc(PT_L2_BASE));
        // L2[0] → L3
        write_pt_desc(gm, PT_L2_BASE, table_desc(PT_L3_BASE));
        // L3[0] → page at DATA_GPA
        write_pt_desc(gm, PT_L3_BASE, page_desc(DATA_GPA));
    }

    fn make_shared_state(gm: &GuestMemory) -> Arc<SmmuSharedState> {
        let state = SmmuSharedState::new(
            gm.clone(),
            40,
            crate::SmmuOasPolicy::Fixed(40),
            false,
            None,
            None,
        );
        state.set_strtab(STRTAB_BASE, STRTAB_LOG2SIZE);
        state.set_enabled(true);
        // Enable EVTQ so fault events are written to guest memory.
        state.set_evtq_config(EVTQ_BASE, EVTQ_LOG2SIZE);
        state.set_evtq_enabled(true);
        state
    }

    /// Count events in the EVTQ by reading EVTQ_PROD from shared state.
    fn evtq_event_count(state: &SmmuSharedState) -> u32 {
        state.evtq_prod()
    }

    // =========================================================================
    // TranslatingMemory tests
    // =========================================================================

    #[test]
    fn test_translating_memory_basic_read() {
        let gm = GuestMemory::allocate(0x60_0000);
        let sid = expected_sid();
        setup_translation(&gm, sid);

        // Write test data at the physical GPA.
        let data = b"hello SMMU";
        gm.write_at(DATA_GPA, data).unwrap();

        let state = make_shared_state(&gm);
        let bus_range = make_bus_range();
        let mock_msi = MockSignalMsi::new();

        let (translating_gm, _msi) =
            device_context(&state, bus_range, TEST_STREAM_ID_BASE, &gm, mock_msi);

        // Read via IOVA 0 → should get data from DATA_GPA.
        let mut buf = vec![0u8; data.len()];
        translating_gm.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf, data);
    }

    #[test]
    fn test_translating_memory_basic_write() {
        let gm = GuestMemory::allocate(0x60_0000);
        let sid = expected_sid();
        setup_translation(&gm, sid);

        let state = make_shared_state(&gm);
        let bus_range = make_bus_range();
        let mock_msi = MockSignalMsi::new();

        let (translating_gm, _msi) =
            device_context(&state, bus_range, TEST_STREAM_ID_BASE, &gm, mock_msi);

        // Write via IOVA.
        let data = b"write test";
        translating_gm.write_at(0, data).unwrap();

        // Verify data appears at the physical GPA.
        let mut buf = vec![0u8; data.len()];
        gm.read_at(DATA_GPA, &mut buf).unwrap();
        assert_eq!(&buf, data);
    }

    #[test]
    fn test_translating_memory_with_offset() {
        let gm = GuestMemory::allocate(0x60_0000);
        let sid = expected_sid();
        setup_translation(&gm, sid);

        // Write data at GPA + 0x100.
        let data = b"offset data";
        gm.write_at(DATA_GPA + 0x100, data).unwrap();

        let state = make_shared_state(&gm);
        let bus_range = make_bus_range();
        let mock_msi = MockSignalMsi::new();

        let (translating_gm, _msi) =
            device_context(&state, bus_range, TEST_STREAM_ID_BASE, &gm, mock_msi);

        // Read via IOVA 0x100 → DATA_GPA + 0x100.
        let mut buf = vec![0u8; data.len()];
        translating_gm.read_at(0x100, &mut buf).unwrap();
        assert_eq!(&buf, data);
    }

    #[test]
    fn test_translating_memory_cross_page() {
        let gm = GuestMemory::allocate(0x60_0000);
        let sid = expected_sid();

        // Set up STE and CD.
        write_ste(&gm, sid, &make_s1_ste(CD_BASE));
        write_cd(&gm, CD_BASE, 0);

        // Map two adjacent pages:
        // L3[0] → DATA_GPA (page at IOVA 0x0000)
        // L3[1] → DATA_GPA + 0x2000 (page at IOVA 0x1000)
        write_pt_desc(&gm, PT_L1_BASE, table_desc(PT_L2_BASE));
        write_pt_desc(&gm, PT_L2_BASE, table_desc(PT_L3_BASE));
        write_pt_desc(&gm, PT_L3_BASE, page_desc(DATA_GPA));
        write_pt_desc(&gm, PT_L3_BASE + 8, page_desc(DATA_GPA + 0x2000));

        // Write data spanning the page boundary.
        let data_page1 = vec![0xAAu8; 0x10];
        let data_page2 = vec![0xBBu8; 0x10];
        gm.write_at(DATA_GPA + 0xFF0, &data_page1).unwrap();
        gm.write_at(DATA_GPA + 0x2000, &data_page2).unwrap();

        let state = make_shared_state(&gm);
        let bus_range = make_bus_range();
        let mock_msi = MockSignalMsi::new();

        let (translating_gm, _msi) =
            device_context(&state, bus_range, TEST_STREAM_ID_BASE, &gm, mock_msi);

        // Read 32 bytes starting at IOVA 0xFF0, crossing into page 2.
        let mut buf = vec![0u8; 0x20];
        translating_gm.read_at(0xFF0, &mut buf).unwrap();
        assert_eq!(&buf[..0x10], &data_page1);
        assert_eq!(&buf[0x10..], &data_page2);
    }

    #[test]
    fn test_translating_memory_bypass() {
        let gm = GuestMemory::allocate(0x60_0000);
        let sid = expected_sid();

        // STE in bypass mode.
        write_ste(&gm, sid, &make_bypass_ste());

        // Write data at GPA 0x1000.
        let data = b"bypass data";
        gm.write_at(0x1000, data).unwrap();

        let state = make_shared_state(&gm);
        let bus_range = make_bus_range();
        let mock_msi = MockSignalMsi::new();

        let (translating_gm, _msi) =
            device_context(&state, bus_range, TEST_STREAM_ID_BASE, &gm, mock_msi);

        // Read via IOVA = GPA (identity mapping in bypass mode).
        let mut buf = vec![0u8; data.len()];
        translating_gm.read_at(0x1000, &mut buf).unwrap();
        assert_eq!(&buf, data);
    }

    #[test]
    fn test_translating_memory_abort() {
        let gm = GuestMemory::allocate(0x60_0000);
        let sid = expected_sid();

        // STE in abort mode (Config=0b000).
        write_ste(&gm, sid, &make_abort_ste());

        let state = make_shared_state(&gm);
        let bus_range = make_bus_range();
        let mock_msi = MockSignalMsi::new();

        let (translating_gm, _msi) =
            device_context(&state, bus_range, TEST_STREAM_ID_BASE, &gm, mock_msi);

        // Read should fail.
        let mut buf = vec![0u8; 4];
        let result = translating_gm.read_at(0, &mut buf);
        assert!(result.is_err());

        // Per the SMMUv3 STE.Config table, Config=0b000 aborts with **no**
        // event recorded.
        assert_eq!(evtq_event_count(&state), 0);
    }

    #[test]
    fn test_translating_memory_illegal_config_records_event() {
        let gm = GuestMemory::allocate(0x60_0000);
        let sid = expected_sid();

        // STE with Config=0b110 (stage-2 translate). This SMMU advertises
        // IDR0.S2P=0, so the STE is ILLEGAL and must fault with C_BAD_STE.
        let ste = Ste {
            qw0: SteDw0::new()
                .with_v(true)
                .with_config(SteConfig::S2_TRANS.0),
            qw1: SteDw1::new(),
            _qw2_7: [0; 6],
        };
        write_ste(&gm, sid, &ste);

        let state = make_shared_state(&gm);
        let bus_range = make_bus_range();
        let mock_msi = MockSignalMsi::new();

        let (translating_gm, _msi) =
            device_context(&state, bus_range, TEST_STREAM_ID_BASE, &gm, mock_msi);

        // Read should fail, and an event should be recorded.
        let mut buf = vec![0u8; 4];
        assert!(translating_gm.read_at(0, &mut buf).is_err());
        assert_eq!(evtq_event_count(&state), 1);
    }

    #[test]
    fn test_translating_memory_unmapped() {
        let gm = GuestMemory::allocate(0x60_0000);
        let sid = expected_sid();

        // Set up STE and CD, but NO page table entries (L1 is all zeros).
        write_ste(&gm, sid, &make_s1_ste(CD_BASE));
        write_cd(&gm, CD_BASE, 0);
        // L1 is all zeros → translation fault.

        let state = make_shared_state(&gm);
        let bus_range = make_bus_range();
        let mock_msi = MockSignalMsi::new();

        let (translating_gm, _msi) =
            device_context(&state, bus_range, TEST_STREAM_ID_BASE, &gm, mock_msi);

        let mut buf = vec![0u8; 4];
        let result = translating_gm.read_at(0, &mut buf);
        assert!(result.is_err());

        // Should have written a fault event to the EVTQ.
        assert_eq!(evtq_event_count(&state), 1);
        // Read the event from the EVTQ in guest memory.
        let written: EvtEntry = gm.read_plain(EVTQ_BASE).expect("read event");
        assert_eq!(written.event_id(), EventId::F_TRANSLATION);
    }

    #[test]
    fn test_translating_memory_unassigned_bus() {
        let gm = GuestMemory::allocate(0x60_0000);

        let state = make_shared_state(&gm);
        // Bus range NOT assigned (secondary_bus = 0) → RID = 0.
        // With SMMU enabled, stream ID 0 has no valid STE → fault.
        let bus_range = AssignedBusRange::new();
        let mock_msi = MockSignalMsi::new();

        let (translating_gm, _msi) =
            device_context(&state, bus_range, TEST_STREAM_ID_BASE, &gm, mock_msi);

        // Should fault because STE 0 is not configured.
        let mut buf = vec![0u8; 10];
        translating_gm.read_at(0x2000, &mut buf).unwrap_err();
    }

    #[test]
    fn test_translating_memory_smmu_disabled() {
        let gm = GuestMemory::allocate(0x60_0000);

        // Write data at GPA 0x3000.
        let data = b"disabled smmu";
        gm.write_at(0x3000, data).unwrap();

        let state = SmmuSharedState::new(
            gm.clone(),
            40,
            crate::SmmuOasPolicy::Fixed(40),
            false,
            None,
            None,
        );
        let bus_range = make_bus_range();
        let mock_msi = MockSignalMsi::new();

        let (translating_gm, _msi) =
            device_context(&state, bus_range, TEST_STREAM_ID_BASE, &gm, mock_msi);

        // Should bypass translation.
        let mut buf = vec![0u8; data.len()];
        translating_gm.read_at(0x3000, &mut buf).unwrap();
        assert_eq!(&buf, data);
    }

    // =========================================================================
    // SmmuSignalMsi tests
    // =========================================================================

    #[test]
    fn test_signal_msi_translated() {
        let gm = GuestMemory::allocate(0x60_0000);
        let sid = expected_sid();
        setup_translation(&gm, sid);

        // Also map a doorbell page: IOVA 0x800 → DATA_GPA + 0x1000.
        write_pt_desc(&gm, PT_L3_BASE + 8, page_desc(DATA_GPA + 0x1000));

        let state = make_shared_state(&gm);
        let bus_range = make_bus_range();
        let mock_msi = MockSignalMsi::new();

        let (_gm, smmu_msi) = device_context(
            &state,
            bus_range,
            TEST_STREAM_ID_BASE,
            &gm,
            mock_msi.clone(),
        );

        // Fire MSI with IOVA address 0x1040 (page 1 + offset 0x40).
        // devid is a RID — the SMMU combines it with segment to get the SID.
        smmu_msi.signal_msi(Some(TEST_RID), 0x1040, 0xDEAD);

        let calls = mock_msi.take_calls();
        assert_eq!(calls.len(), 1);
        // Translated address: DATA_GPA + 0x1000 + 0x40.
        assert_eq!(calls[0], (Some(TEST_RID), DATA_GPA + 0x1040, 0xDEAD));
    }

    #[test]
    fn test_signal_msi_bypass() {
        let gm = GuestMemory::allocate(0x60_0000);
        let sid = expected_sid();

        write_ste(&gm, sid, &make_bypass_ste());

        let state = make_shared_state(&gm);
        let bus_range = make_bus_range();
        let mock_msi = MockSignalMsi::new();

        let (_gm, smmu_msi) = device_context(
            &state,
            bus_range,
            TEST_STREAM_ID_BASE,
            &gm,
            mock_msi.clone(),
        );

        // MsiTarget resolves devid to a BDF before calling SmmuSignalMsi.
        smmu_msi.signal_msi(Some(TEST_RID), 0xFEE0_0000, 0x42);

        let calls = mock_msi.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], (Some(TEST_RID), 0xFEE0_0000, 0x42));
    }

    #[test]
    fn test_signal_msi_unmapped() {
        let gm = GuestMemory::allocate(0x60_0000);
        let sid = expected_sid();

        // STE with S1 translation, but no page table entries.
        write_ste(&gm, sid, &make_s1_ste(CD_BASE));
        write_cd(&gm, CD_BASE, 0);

        let state = make_shared_state(&gm);
        let bus_range = make_bus_range();
        let mock_msi = MockSignalMsi::new();

        let (_gm, smmu_msi) = device_context(
            &state,
            bus_range,
            TEST_STREAM_ID_BASE,
            &gm,
            mock_msi.clone(),
        );

        // Fire MSI with unmapped address. devid is a RID.
        smmu_msi.signal_msi(Some(TEST_RID), 0x1000, 0x42);

        // MSI should NOT be forwarded.
        let calls = mock_msi.take_calls();
        assert!(calls.is_empty());

        // Fault event should be written to the EVTQ.
        assert_eq!(evtq_event_count(&state), 1);
    }

    #[test]
    fn test_signal_msi_devid_passthrough() {
        let gm = GuestMemory::allocate(0x60_0000);
        let sid = expected_sid();

        write_ste(&gm, sid, &make_bypass_ste());

        let state = make_shared_state(&gm);
        let bus_range = make_bus_range();
        let mock_msi = MockSignalMsi::new();

        let (_gm, smmu_msi) = device_context(
            &state,
            bus_range,
            TEST_STREAM_ID_BASE,
            &gm,
            mock_msi.clone(),
        );

        // devid (RID) should be passed through unchanged to the inner MSI.
        smmu_msi.signal_msi(Some(TEST_RID), 0x1000, 0x42);

        let calls = mock_msi.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, Some(TEST_RID));
    }

    #[test]
    fn test_signal_msi_no_devid() {
        let gm = GuestMemory::allocate(0x60_0000);

        let state = make_shared_state(&gm);
        let bus_range = make_bus_range();
        let mock_msi = MockSignalMsi::new();

        let (_gm, smmu_msi) = device_context(
            &state,
            bus_range,
            TEST_STREAM_ID_BASE,
            &gm,
            mock_msi.clone(),
        );

        // devid=None means no BDF — MSI should be dropped.
        smmu_msi.signal_msi(None, 0xFEE0_0000, 0x42);

        let calls = mock_msi.take_calls();
        assert_eq!(calls.len(), 0);
    }

    // =========================================================================
    // Stream ID remapping tests (non-zero stream_id_base)
    // =========================================================================

    #[test]
    fn test_translating_memory_nonzero_stream_id_base() {
        let gm = GuestMemory::allocate(0x60_0000);

        // Use a non-zero stream_id_base (simulating a second root complex
        // with its own region in the SMMU stream table).
        // stream_id_base=256, bus=1 → SID = 256 + 256 = 512 (within 1024).
        let stream_id_base: u32 = 256;
        let bus: u8 = 1;
        let sid = stream_id_base + ((bus as u32) << 8);

        // Set up translation for the remapped stream ID.
        write_ste(&gm, sid, &make_s1_ste(CD_BASE));
        write_cd(&gm, CD_BASE, 0);
        write_pt_desc(&gm, PT_L1_BASE, table_desc(PT_L2_BASE));
        write_pt_desc(&gm, PT_L2_BASE, table_desc(PT_L3_BASE));
        write_pt_desc(&gm, PT_L3_BASE, page_desc(DATA_GPA));

        let data = b"remapped sid test";
        gm.write_at(DATA_GPA, data).unwrap();

        let state = make_shared_state(&gm);
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(bus, bus);
        let mock_msi = MockSignalMsi::new();

        let (translating_gm, _msi) =
            device_context(&state, bus_range, stream_id_base, &gm, mock_msi);

        // Read via IOVA 0 → should find the STE at the remapped stream ID.
        let mut buf = vec![0u8; data.len()];
        translating_gm.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf, data);
    }

    #[test]
    fn test_signal_msi_nonzero_stream_id_base() {
        let gm = GuestMemory::allocate(0x60_0000);

        // Non-zero base (different root complex).
        let stream_id_base: u32 = 256;
        let bus: u8 = 1;
        let sid = stream_id_base + ((bus as u32) << 8);

        // Set up bypass STE for the remapped stream ID.
        write_ste(&gm, sid, &make_bypass_ste());

        let state = make_shared_state(&gm);
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(bus, bus);
        let mock_msi = MockSignalMsi::new();

        let (_gm, smmu_msi) =
            device_context(&state, bus_range, stream_id_base, &gm, mock_msi.clone());

        // Fire MSI — bypass mode means address passes through unchanged.
        let rid = (bus as u32) << 8;
        smmu_msi.signal_msi(Some(rid), 0xFEE0_0000, 0x99);

        let calls = mock_msi.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], (Some(rid), 0xFEE0_0000, 0x99));
    }

    // =========================================================================
    // resolve_host_caps (accel host/guest compatibility) tests
    // =========================================================================

    /// A `HostSmmuCaps` that is compatible with everything the emulator
    /// advertises (AArch64, little-endian, 4K granule, ample OAS).
    fn compatible_host_caps() -> crate::HostSmmuCaps {
        crate::HostSmmuCaps {
            oas: Ips::IPS_48,
            ttf: registers::Idr0Ttf::new().with_aarch64(true),
            ttendian: registers::Idr0TtEndian::LE,
            gran4k: true,
        }
    }

    /// An accel-mode shared state with the given OAS policy.
    fn make_accel_state(policy: crate::SmmuOasPolicy) -> Arc<SmmuSharedState> {
        let gm = GuestMemory::allocate(0x1000);
        SmmuSharedState::new(gm, 40, policy, true, None, None)
    }

    #[test]
    fn resolve_host_caps_accepts_compatible_host() {
        let state = make_accel_state(crate::SmmuOasPolicy::Fixed(40));
        state.resolve_host_caps(compatible_host_caps()).unwrap();
    }

    #[test]
    fn resolve_host_caps_auto_adopts_host_oas() {
        let state = make_accel_state(crate::SmmuOasPolicy::Auto { provisional: 40 });
        let caps = crate::HostSmmuCaps {
            oas: Ips::IPS_48,
            ..compatible_host_caps()
        };
        state.resolve_host_caps(caps).unwrap();
        assert_eq!(state.oas_bits(), 48);
    }

    #[test]
    fn resolve_host_caps_rejects_fixed_oas_above_host() {
        let state = make_accel_state(crate::SmmuOasPolicy::Fixed(52));
        let caps = crate::HostSmmuCaps {
            oas: Ips::IPS_44,
            ..compatible_host_caps()
        };
        let err = state.resolve_host_caps(caps).unwrap_err().to_string();
        assert!(err.contains("exceeds host SMMU OAS"), "{err}");
    }

    #[test]
    fn resolve_host_caps_rejects_no_aarch64() {
        let state = make_accel_state(crate::SmmuOasPolicy::Fixed(40));
        // AArch32-only host (TTF bit for AArch64 not set).
        let caps = crate::HostSmmuCaps {
            ttf: registers::Idr0Ttf::new().with_aarch32(true),
            ..compatible_host_caps()
        };
        let err = state.resolve_host_caps(caps).unwrap_err().to_string();
        assert!(err.contains("AArch64"), "{err}");
    }

    #[test]
    fn resolve_host_caps_accepts_aarch32_and_aarch64_host() {
        // A host advertising both formats supports AArch64 — must be accepted.
        let state = make_accel_state(crate::SmmuOasPolicy::Fixed(40));
        let caps = crate::HostSmmuCaps {
            ttf: registers::Idr0Ttf::new()
                .with_aarch32(true)
                .with_aarch64(true),
            ..compatible_host_caps()
        };
        state.resolve_host_caps(caps).unwrap();
    }

    #[test]
    fn resolve_host_caps_rejects_big_endian_only_host() {
        let state = make_accel_state(crate::SmmuOasPolicy::Fixed(40));
        let caps = crate::HostSmmuCaps {
            ttendian: registers::Idr0TtEndian::BE,
            ..compatible_host_caps()
        };
        let err = state.resolve_host_caps(caps).unwrap_err().to_string();
        assert!(err.contains("little-endian"), "{err}");
    }

    #[test]
    fn resolve_host_caps_accepts_mixed_endian_host() {
        // Mixed-endian host supports little-endian — must be accepted.
        let state = make_accel_state(crate::SmmuOasPolicy::Fixed(40));
        let caps = crate::HostSmmuCaps {
            ttendian: registers::Idr0TtEndian::MIXED,
            ..compatible_host_caps()
        };
        state.resolve_host_caps(caps).unwrap();
    }

    #[test]
    fn resolve_host_caps_rejects_no_gran4k() {
        let state = make_accel_state(crate::SmmuOasPolicy::Fixed(40));
        let caps = crate::HostSmmuCaps {
            gran4k: false,
            ..compatible_host_caps()
        };
        let err = state.resolve_host_caps(caps).unwrap_err().to_string();
        assert!(err.contains("4KB translation granule"), "{err}");
    }

    #[test]
    fn resolve_host_caps_rejects_second_device_with_different_caps() {
        let state = make_accel_state(crate::SmmuOasPolicy::Fixed(40));
        state.resolve_host_caps(compatible_host_caps()).unwrap();
        // A second device backed by a different physical SMMU (different OAS).
        let other = crate::HostSmmuCaps {
            oas: Ips::IPS_44,
            ..compatible_host_caps()
        };
        let err = state.resolve_host_caps(other).unwrap_err().to_string();
        assert!(
            err.contains("cannot be backed by two physical SMMUs"),
            "{err}"
        );
    }

    #[test]
    fn resolve_host_caps_accepts_second_device_with_identical_caps() {
        let state = make_accel_state(crate::SmmuOasPolicy::Fixed(40));
        state.resolve_host_caps(compatible_host_caps()).unwrap();
        // Same caps again (another device behind the same physical SMMU).
        state.resolve_host_caps(compatible_host_caps()).unwrap();
    }

    // =========================================================================
    // Disabled-state policy (GBPA.ABORT) tests
    // =========================================================================

    /// Non-accel: while the SMMU is disabled, DMA bypasses (IOVA = GPA) when
    /// `GBPA.ABORT=0`.
    #[test]
    fn test_disabled_bypass_when_gbpa_abort_clear() {
        let gm = GuestMemory::allocate(0x60_0000);
        let data = b"disabled-bypass";
        gm.write_at(0x3000, data).unwrap();

        let state = SmmuSharedState::new(
            gm.clone(),
            40,
            crate::SmmuOasPolicy::Fixed(40),
            false,
            None,
            None,
        );
        // Disabled with GBPA.ABORT=0 (the reset default).
        state.set_gbpa_abort(false);
        // Enable the EVTQ so an (unexpected) abort would be observable.
        state.set_evtq_config(EVTQ_BASE, EVTQ_LOG2SIZE);
        state.set_evtq_enabled(true);

        let bus_range = make_bus_range();
        let mock_msi = MockSignalMsi::new();
        let (translating_gm, _msi) =
            device_context(&state, bus_range, TEST_STREAM_ID_BASE, &gm, mock_msi);

        let mut buf = vec![0u8; data.len()];
        translating_gm.read_at(0x3000, &mut buf).unwrap();
        assert_eq!(&buf, data);
        assert_eq!(evtq_event_count(&state), 0);
    }

    /// Non-accel: while the SMMU is disabled, DMA aborts when `GBPA.ABORT=1`.
    /// Per SMMUv3 a global abort generates **no** event record (there is no
    /// stream context to fault against), so the EVTQ stays empty even though
    /// it is enabled.
    #[test]
    fn test_disabled_abort_when_gbpa_abort_set() {
        let gm = GuestMemory::allocate(0x60_0000);

        let state = SmmuSharedState::new(
            gm.clone(),
            40,
            crate::SmmuOasPolicy::Fixed(40),
            false,
            None,
            None,
        );
        // Disabled with GBPA.ABORT=1.
        state.set_gbpa_abort(true);
        state.set_evtq_config(EVTQ_BASE, EVTQ_LOG2SIZE);
        state.set_evtq_enabled(true);

        let bus_range = make_bus_range();
        let mock_msi = MockSignalMsi::new();
        let (translating_gm, _msi) =
            device_context(&state, bus_range, TEST_STREAM_ID_BASE, &gm, mock_msi);

        let mut buf = vec![0u8; 4];
        translating_gm.read_at(0x3000, &mut buf).unwrap_err();
        // A global (GBPA) abort generates no event record.
        assert_eq!(evtq_event_count(&state), 0);
    }

    // =========================================================================
    // current_stream_config tests
    // =========================================================================

    #[test]
    fn test_current_stream_config_disabled_bypass() {
        let gm = GuestMemory::allocate(0x60_0000);
        let state = SmmuSharedState::new(
            gm.clone(),
            40,
            crate::SmmuOasPolicy::Fixed(40),
            true,
            None,
            None,
        );
        // Disabled, GBPA.ABORT=0 → Bypass, regardless of SID.
        state.set_gbpa_abort(false);
        assert_eq!(state.current_stream_config(0), StreamConfig::Bypass);
        assert_eq!(state.current_stream_config(0x1234), StreamConfig::Bypass);
    }

    #[test]
    fn test_current_stream_config_disabled_abort() {
        let gm = GuestMemory::allocate(0x60_0000);
        let state = SmmuSharedState::new(
            gm.clone(),
            40,
            crate::SmmuOasPolicy::Fixed(40),
            true,
            None,
            None,
        );
        // Disabled, GBPA.ABORT=1 → Abort, regardless of SID.
        state.set_gbpa_abort(true);
        assert_eq!(state.current_stream_config(0), StreamConfig::Abort);
        assert_eq!(state.current_stream_config(0x1234), StreamConfig::Abort);
    }

    #[test]
    fn test_current_stream_config_enabled_reads_ste() {
        let gm = GuestMemory::allocate(0x60_0000);
        let sid = expected_sid();
        let state = make_shared_state(&gm);

        // Valid S1_TRANS STE → Translate, carrying this SID.
        write_ste(&gm, sid, &make_s1_ste(CD_BASE));
        assert!(matches!(
            state.current_stream_config(sid),
            StreamConfig::Translate { sid: s, .. } if s == sid
        ));

        // Bypass STE → Bypass.
        write_ste(&gm, sid, &make_bypass_ste());
        assert_eq!(state.current_stream_config(sid), StreamConfig::Bypass);

        // Abort STE → Abort.
        write_ste(&gm, sid, &make_abort_ste());
        assert_eq!(state.current_stream_config(sid), StreamConfig::Abort);

        // Invalid STE (V=0) → Abort.
        write_ste(
            &gm,
            sid,
            &Ste {
                qw0: SteDw0::new().with_v(false),
                qw1: SteDw1::new(),
                _qw2_7: [0; 6],
            },
        );
        assert_eq!(state.current_stream_config(sid), StreamConfig::Abort);

        // Illegal config (0b110 stage-2 on a stage-1-only SMMU) → Abort. The
        // config plane is pure: no fault event is synthesized here (a C_BAD_STE
        // is a data-plane fault, delivered via the software translate path or
        // the host VEVENTQ).
        write_ste(
            &gm,
            sid,
            &Ste {
                qw0: SteDw0::new()
                    .with_v(true)
                    .with_config(SteConfig::S2_TRANS.0),
                qw1: SteDw1::new(),
                _qw2_7: [0; 6],
            },
        );
        assert_eq!(state.current_stream_config(sid), StreamConfig::Abort);
    }

    #[test]
    fn test_current_stream_config_out_of_range_sid_aborts() {
        let gm = GuestMemory::allocate(0x60_0000);
        let state = make_shared_state(&gm);
        // strtab has 2^STRTAB_LOG2SIZE entries; an SID past the end aborts.
        let oob_sid = 1u32 << STRTAB_LOG2SIZE;
        assert_eq!(state.current_stream_config(oob_sid), StreamConfig::Abort);
    }

    // =========================================================================
    // register_accel_device initial-policy tests
    // =========================================================================

    /// A mock accel backend that records the configs applied to it.
    struct MockBackend {
        configs: Mutex<Vec<StreamConfig>>,
    }

    impl MockBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                configs: Mutex::new(Vec::new()),
            })
        }

        fn take(&self) -> Vec<StreamConfig> {
            std::mem::take(&mut *self.configs.lock())
        }
    }

    impl AcceleratedStreamBackend for MockBackend {
        fn set_stream_config(&self, config: StreamConfig) -> anyhow::Result<()> {
            self.configs.lock().push(config);
            Ok(())
        }
    }

    fn make_accel_shared(gm: &GuestMemory) -> Arc<SmmuSharedState> {
        SmmuSharedState::new(
            gm.clone(),
            40,
            crate::SmmuOasPolicy::Fixed(40),
            true,
            None,
            None,
        )
    }

    /// Registering a device while the SMMU is disabled (GBPA.ABORT=0) applies
    /// Bypass immediately, even before the bus is assigned.
    #[test]
    fn test_register_applies_bypass_when_disabled() {
        let gm = GuestMemory::allocate(0x60_0000);
        let state = make_accel_shared(&gm);
        state.set_gbpa_abort(false);

        let backend = MockBackend::new();
        // Bus not yet assigned.
        let bus_range = AssignedBusRange::new();
        state.register_accel_device(bus_range, TEST_STREAM_ID_BASE, backend.clone());

        let applied = backend.take();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0], StreamConfig::Bypass);
    }

    /// Registering a device while the SMMU is disabled (GBPA.ABORT=1) applies
    /// Abort immediately.
    #[test]
    fn test_register_applies_abort_when_disabled_gbpa_abort() {
        let gm = GuestMemory::allocate(0x60_0000);
        let state = make_accel_shared(&gm);
        state.set_gbpa_abort(true);

        let backend = MockBackend::new();
        let bus_range = AssignedBusRange::new();
        state.register_accel_device(bus_range, TEST_STREAM_ID_BASE, backend.clone());

        let applied = backend.take();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0], StreamConfig::Abort);
    }

    /// Registering a device while the SMMU is enabled with an assigned bus
    /// applies the stream's current STE-derived policy.
    #[test]
    fn test_register_applies_ste_policy_when_enabled() {
        let gm = GuestMemory::allocate(0x60_0000);
        let state = make_accel_shared(&gm);
        state.set_strtab(STRTAB_BASE, STRTAB_LOG2SIZE);
        state.set_enabled(true);

        let sid = expected_sid();
        write_ste(&gm, sid, &make_bypass_ste());

        let backend = MockBackend::new();
        let bus_range = make_bus_range(); // assigned
        state.register_accel_device(bus_range, TEST_STREAM_ID_BASE, backend.clone());

        let applied = backend.take();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0], StreamConfig::Bypass);
    }

    /// Registering a device while the SMMU is enabled but the bus is not yet
    /// assigned leaves the device fail-closed (no initial apply); a later
    /// CFGI_STE (apply_stream_config) catches it up.
    #[test]
    fn test_register_enabled_unassigned_bus_then_cfgi() {
        let gm = GuestMemory::allocate(0x60_0000);
        let state = make_accel_shared(&gm);
        state.set_strtab(STRTAB_BASE, STRTAB_LOG2SIZE);
        state.set_enabled(true);

        let backend = MockBackend::new();
        let bus_range = AssignedBusRange::new(); // unassigned
        state.register_accel_device(bus_range.clone(), TEST_STREAM_ID_BASE, backend.clone());
        // No config applied yet (fail-closed / detached).
        assert!(backend.take().is_empty());

        // Guest assigns the bus and programs the STE, then issues CFGI_STE.
        bus_range.set_bus_range(TEST_BUS, TEST_BUS);
        let sid = expected_sid();
        write_ste(&gm, sid, &make_s1_ste(CD_BASE));
        state.apply_stream_config(sid);

        let applied = backend.take();
        assert_eq!(applied.len(), 1);
        assert!(
            matches!(applied[0], StreamConfig::Translate { sid: s, .. } if s == sid),
            "expected Translate for sid {sid:#x}, got {:?}",
            applied[0]
        );
    }

    /// apply_all_stream_configs re-drives every registered backend (used for
    /// GBPA writes, SMMUEN transitions, and CFGI_ALL).
    #[test]
    fn test_apply_all_stream_configs_redrives() {
        let gm = GuestMemory::allocate(0x60_0000);
        let state = make_accel_shared(&gm);
        state.set_strtab(STRTAB_BASE, STRTAB_LOG2SIZE);
        state.set_gbpa_abort(false);

        let backend = MockBackend::new();
        let bus_range = make_bus_range();
        state.register_accel_device(bus_range, TEST_STREAM_ID_BASE, backend.clone());
        // Initial register applied Bypass (disabled, GBPA.ABORT=0).
        assert_eq!(backend.take().last().copied(), Some(StreamConfig::Bypass));

        // Enable the SMMU and program an abort STE, then re-drive.
        let sid = expected_sid();
        write_ste(&gm, sid, &make_abort_ste());
        state.set_enabled(true);
        state.apply_all_stream_configs();

        let applied = backend.take();
        assert_eq!(applied.last().copied(), Some(StreamConfig::Abort));
    }
}
