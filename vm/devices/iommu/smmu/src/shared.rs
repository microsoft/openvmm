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

use crate::spec::events::EventId;
use crate::spec::events::EvtEntry;
use crate::spec::registers;
use crate::translate;
use guestmem::GuestMemory;
use pal_event::Event;
use parking_lot::Mutex;
use parking_lot::RwLock;
use pci_core::msi::SignalMsi;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Weak;
use vmcore::irqfd::IrqFd;
use vmcore::irqfd::IrqFdRoute;
use vmcore::line_interrupt::LineInterrupt;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

/// The context a host-assignment backend needs to wire a VFIO device into
/// this emulated SMMU for hardware-accelerated nested stage-1 translation.
///
/// This is the concrete type carried (type-erased) by
/// [`DmaPassthrough::HardwareNestable`](pci_core::dma::DmaPassthrough::HardwareNestable)
/// for devices behind an accel-capable SMMU. The VFIO resolver downcasts the
/// opaque handle to this type, binds the vSMMU to the host hardware
/// ([`bind_accel_viommu`]), and blocks the device until PCI routing gives it a
/// BDF to derive a StreamID from ([`register_accel_device`]).
///
/// [`bind_accel_viommu`]: SmmuSharedState::bind_accel_viommu
/// [`register_accel_device`]: SmmuSharedState::register_accel_device
#[derive(Clone)]
pub struct SmmuNestingContext {
    /// Shared state of the emulated SMMU this device sits behind.
    pub shared: Arc<SmmuSharedState>,
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
/// which is vIOMMU-scoped, lives on [`Invalidate`] instead.
///
/// A backend is registered against one StreamID for its whole life. The guest
/// moving a device to a different BDF retires the registration and creates a
/// new one, so nothing here ever has to learn about a StreamID change.
pub trait AcceleratedStreamBackend: Send + Sync {
    /// The guest reconfigured this stream's STE (via `CFGI_STE`), or the
    /// emulator recomputed the stream's policy (e.g. on a `GBPA` write or
    /// `SMMUEN` transition). The emulator has already parsed and validated
    /// the STE into `config`.
    ///
    /// On error, the backend must leave its previously applied configuration
    /// active. The emulator preserves the corresponding forwarding state and
    /// reports a guest configuration command as a command-queue abort.
    fn set_stream_config(&self, config: StreamConfig) -> anyhow::Result<()>;
}

/// Invalidates cached state for one accelerated emulated SMMU.
///
/// The emulator binds exactly one implementation to an accelerated SMMU and
/// forwards each guest invalidation once, regardless of how many streams are
/// registered. Consecutive forwardable CMDQ commands are delivered as one
/// ordered batch at synchronization and configuration boundaries.
///
/// Each entry is the raw 128-bit CMDQ command as a `[qw0, qw1]` quadword pair.
/// The implementation must interpret only commands supported by the emulator's
/// advertised capabilities.
///
/// The emulator calls this with the accelerated registration table locked, so an
/// implementation must not re-enter [`SmmuSharedState`].
pub trait Invalidate: Send + Sync {
    /// Processes `entries` as one ordered batch, preserving program order.
    ///
    /// Returns `Ok(())` if the implementation handled the entire batch.
    /// Returns `Err(handled)` if it did not, where `handled` is the number of
    /// leading entries accepted, so the command at index `handled` is the
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
    /// Stage-1 translation (`Config=S1_TRANS`). Carries the raw stage-1 STE
    /// double-words; the backend already knows the StreamID it was registered
    /// against.
    Translate {
        /// Canonical stage-1 STE double-words `[DW0, DW1]`: the guest STE
        /// reduced to the fields that are architecturally meaningful under this
        /// vSMMU's advertised capabilities, with every RES0/IGNORED field
        /// zeroed (see `canonical_s1_ste_dwords`).
        ste_dwords: [u64; 2],
    },
}

/// Policy inputs used to compute one stream's effective host attachment.
///
/// Keeping this separate from [`SharedStateInner`] lets register updates
/// evaluate and install a proposed policy before publishing it to DMA paths.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TranslationPolicy {
    pub(crate) enabled: bool,
    pub(crate) gbpa_abort: bool,
    pub(crate) strtab_base: u64,
    pub(crate) strtab_log2size: u8,
    pub(crate) oas_mask: u64,
    pub(crate) ssid_bits: u8,
    pub(crate) ats: bool,
}

/// Reduce a guest stage-1 STE to the double-words that are architecturally
/// meaningful under this vSMMU's advertised capabilities, zeroing every field
/// that is RES0 or IGNORED and truncating `S1ContextPtr` to the OAS.
///
/// This is **architectural canonicalization**: which fields survive is a
/// consequence of the SMMUv3 architecture plus the capabilities this emulator
/// advertises, so it belongs to the emulator rather than any host binding. The
/// result is what a real SMMU with these IDR values would act on, so a consumer
/// that programs hardware from it needs no further filtering.
///
/// Under the current fixed capabilities the dropped fields are all RES0 or
/// IGNORED (IHI 0070H.a §5.2, except where noted):
///
/// - `IDR0.S2P == 0` → all stage-2 fields (S2FWB, S2HWU) are RES0;
/// - `IDR1.ATTR_TYPES_OVR == 0` → MTCFG, MemAttr, SHCFG, ALLOCCFG are RES0;
/// - `IDR1.ATTR_PERMS_OVR == 0` → NSCFG, PRIVCFG, INSTCFG are RES0;
/// - `IDR0.HYP == 0` → STRW is fixed to NS-EL1 (0);
/// - when `IDR0.ATS == 0`, EATS is IGNORED;
/// - when `IDR1.SSIDSIZE == 0`, S1CDMax is IGNORED, and S1Fmt and S1DSS are in
///   turn IGNORED ("substreams unsupported"), leaving `S1ContextPtr`
///   addressing exactly one CD;
/// - unadvertised optional features (S1PIE, S1MPAM, CONT, DCP, DRE, PPAR, MEV)
///   are RES0/IGNORED; the SW bits have no hardware effect.
///
/// `S1ContextPtr` is truncated to the OAS because a real SMMU truncates the CD
/// fetch address to the OAS (§3.4), so the pointer it acts on never contains
/// bits above the advertised output address size.
///
/// Retained unconditionally — DW0: V, Config, S1ContextPtr; DW1: S1CIR, S1COR,
/// S1CSH, S1STALLD. S1Fmt, S1CDMax, and S1DSS are additionally retained when
/// SSID support is advertised; EATS is retained when ATS is advertised.
///
/// NOTE: if this emulator ever advertises one of the above features, the
/// retained set must grow to match (and attach-time capability resolution must
/// gate the new field against the host SMMU).
fn canonical_s1_ste_dwords(
    ste: &crate::spec::ste::Ste,
    oas_mask: u64,
    ssid_bits: u8,
    ats: bool,
) -> [u64; 2] {
    use crate::spec::ste::SteDw0;
    use crate::spec::ste::SteDw1;

    let mut dw0 = SteDw0::new()
        .with_v(ste.qw0.v())
        .with_config(ste.qw0.config())
        .with_s1_context_ptr((ste.s1_context_ptr() & oas_mask) >> 6);

    let mut dw1 = SteDw1::new()
        .with_s1_cir(ste.qw1.s1_cir())
        .with_s1_cor(ste.qw1.s1_cor())
        .with_s1_csh(ste.qw1.s1_csh())
        .with_s1stalld(ste.qw1.s1stalld());

    if ssid_bits != 0 {
        dw0 = dw0
            .with_s1_fmt(ste.qw0.s1_fmt())
            .with_s1_cd_max(ste.qw0.s1_cd_max());
        dw1 = dw1.with_s1_dss(ste.qw1.s1_dss());
    }
    if ats {
        dw1 = dw1.with_eats(ste.qw1.eats());
    }

    [dw0.into(), dw1.into()]
}

/// Registration entry for a VFIO device with iommufd-accelerated translation.
///
/// Keyed by StreamID in [`AccelState`], which the guest's PCI routing assigns:
/// a registration exists exactly while its device has an identity to translate
/// under.
///
/// Stored in [`SmmuSharedState`] so host-side registration and removal can
/// complete while the chipset emulator is stopped.
struct AccelDeviceRegistration {
    /// The iommufd-backed stream handler.
    backend: Arc<dyn AcceleratedStreamBackend>,
    /// Whether this stream is currently attached in translating (`S1_TRANS`)
    /// mode on the host, recorded at apply time rather than recomputed at
    /// query time so it matches the host attach state exactly. It gates
    /// SID-based invalidation forwarding. Access is serialized by the shared
    /// registration-table lock.
    translating: bool,
}

/// Accelerated state for one emulated SMMU: the registered per-device stream
/// backends and the host vIOMMU they all share.
///
/// These live under one lock because they are one unit: an invalidation batch
/// is admitted (by StreamID) and forwarded to the vIOMMU under a single hold,
/// so no rebind or teardown can interleave.
#[derive(Default)]
struct AccelState {
    devices: HashMap<u32, AccelDeviceRegistration>,
    /// The host vIOMMU that every registered backend shares, held weakly.
    ///
    /// The strong references are the backends' own, so the vIOMMU is destroyed
    /// along with the last device behind this SMMU without this table having to
    /// track that itself. `None` until the first accelerated device binds, and
    /// dangling once the last one goes until the next device re-sets it.
    viommu: Option<Weak<dyn Invalidate>>,
}

/// Exclusive access to the accelerated registration table.
///
/// Held across an invalidation batch — from the StreamID check that admits the
/// first entry through the host call that consumes the batch — so that no
/// concurrent StreamID rebind or device teardown can retire a vDevice the batch
/// names. Both run under this same lock.
pub(crate) struct AccelDevices<'a> {
    accel: parking_lot::MutexGuard<'a, AccelState>,
}

impl AccelDevices<'_> {
    /// Whether `sid` is currently attached in translating (`S1_TRANS`) mode,
    /// and therefore has host state its SID-based invalidations can reach.
    pub(crate) fn is_translating(&self, sid: u32) -> bool {
        self.accel
            .devices
            .get(&sid)
            .is_some_and(|reg| reg.translating)
    }
}

/// Handle tying an accelerated registration to the StreamID it was created
/// for. Dropping it synchronously drives the stream to abort and unregisters
/// the backend.
///
/// Held by the assigned PCI device (see the VFIO resolver) for as long as the
/// guest keeps the device at the BDF this StreamID came from; a move to a
/// different BDF drops this and creates a new one. Holds a [`Weak`] reference
/// so it never keeps the emulated SMMU alive: if the SMMU is already gone there
/// is nothing left to unregister from.
#[derive(Debug)]
pub struct AccelRegistration {
    shared: Weak<SmmuSharedState>,
    sid: u32,
}

impl AccelRegistration {
    /// The StreamID this registration was created for, fixed for its life.
    pub fn stream_id(&self) -> u32 {
        self.sid
    }
}

impl Drop for AccelRegistration {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.unregister_accel_device(self.sid);
        }
    }
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
    /// How the advertised OAS is resolved against the host SMMU before
    /// capabilities are frozen (see [`resolve_host_caps`](Self::resolve_host_caps)).
    oas_policy: crate::SmmuOasPolicy,
    /// Accelerated stream registrations and the host vIOMMU they share. This
    /// is shared because VFIO devices can be added or removed while the
    /// chipset emulator is stopped.
    accel_state: Mutex<AccelState>,
}

struct SharedStateInner {
    /// Whether guest-visible identification registers are fixed. Set when the
    /// device first starts and never cleared by stop or reset.
    capabilities_frozen: bool,
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
    /// Advertised PASID/SSID width, resolved from the host before start.
    ssid_bits: u8,
    /// Whether ATS is advertised, resolved from the host before start.
    ats: bool,
    /// Host SMMU capabilities, once an accelerated VFIO device has bound and
    /// [`SmmuSharedState::resolve_host_caps`] has resolved or validated the
    /// host-derived parameters. `None` until then (and always `None` for
    /// non-accel SMMUs).
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
    /// SMMU_EVENTQ_PROD: write index, advanced by the SMMU as it writes
    /// events, plus the overflow flag.
    evtq_prod: registers::EventqProd,
    /// SMMU_EVENTQ_CONS: read index, advanced by the guest via MMIO, plus the
    /// overflow acknowledge flag.
    evtq_cons: registers::EventqCons,

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

fn evtq_index_mask(log2size: u8) -> u32 {
    (1 << (log2size + 1)) - 1
}

/// Whether the Event queue is empty, i.e. the effective producer and consumer
/// indices agree. The overflow handshake in bit 31 is independent of emptiness.
fn evtq_indices_equal(qs: &QueueErrorState) -> bool {
    let mask = evtq_index_mask(qs.evtq_log2size);
    qs.evtq_prod.wr() & mask == qs.evtq_cons.rd() & mask
}

impl SmmuSharedState {
    /// Creates a new shared state with the SMMU disabled.
    ///
    /// `oas_bits` is the initial output address size in bits (e.g., 40 for a
    /// 40-bit physical address space). Computed addresses for STE/CD/PT
    /// fetches are truncated to this width, matching hardware behavior per
    /// SMMUv3 §3.4. `oas_policy` controls whether the value can be resolved
    /// against a host SMMU before capabilities are frozen (see
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
                capabilities_frozen: false,
                enabled: false,
                gbpa_abort: false,
                strtab_base: 0,
                strtab_log2size: 0,
                oas_bits,
                ssid_bits: 0,
                ats: false,
                resolved_host_caps: None,
                oas_mask,
            }),
            guest_memory,
            queue_state: Mutex::new(QueueErrorState {
                evtq_base_addr: 0,
                evtq_log2size: 0,
                evtq_enabled: false,
                evtq_irqen: false,
                evtq_prod: registers::EventqProd::new(),
                evtq_cons: registers::EventqCons::new(),
                gerror: registers::Gerror::new(),
                gerrorn: registers::Gerror::new(),
                gerror_irqen: false,
            }),
            evtq_irq,
            gerror_irq,
            accel,
            oas_policy,
            accel_state: Mutex::new(AccelState::default()),
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

    /// Returns the currently advertised PASID/SSID width.
    pub(crate) fn ssid_bits(&self) -> u8 {
        self.inner.read().ssid_bits
    }

    /// Returns whether ATS is currently advertised.
    pub fn ats_supported(&self) -> bool {
        self.inner.read().ats
    }

    /// Freezes guest-visible capabilities before the VM can observe them.
    pub(crate) fn freeze_capabilities(&self) {
        self.inner.write().capabilities_frozen = true;
    }

    /// Binds this vSMMU to the physical SMMU and host vIOMMU backing an
    /// accelerated device.
    ///
    /// Called when a VFIO device behind this SMMU binds to iommufd, which is
    /// when the backing hardware is first known. The first call validates
    /// host/guest compatibility and resolves mutable pre-start parameters or
    /// validates frozen ones; later calls must name the same physical SMMU
    /// *and* the same vIOMMU, since one vSMMU can span neither two of the former
    /// nor two of the latter.
    ///
    /// The vIOMMU is held weakly: the strong references belong to the stream
    /// backends, so it is released along with the last accelerated device, and
    /// a device hot-plugged after that supplies a freshly built one.
    pub fn bind_accel_viommu<T: Invalidate + 'static>(
        &self,
        caps: crate::HostSmmuCaps,
        viommu: &Arc<T>,
    ) -> anyhow::Result<()> {
        let viommu: Arc<dyn Invalidate> = viommu.clone();
        // Lock order is accel state before `inner`, as in
        // `transition_translation_policy`.
        let mut accel = self.accel_state.lock();
        if let Some(existing) = accel.viommu.as_ref().and_then(Weak::upgrade) {
            if !Arc::ptr_eq(&existing, &viommu) {
                anyhow::bail!(
                    "SMMU is already backed by a live host vIOMMU; a single vSMMU \
                     cannot be backed by two"
                );
            }
        }
        self.resolve_host_caps(caps)?;
        accel.viommu = Some(Arc::downgrade(&viommu));
        Ok(())
    }

    /// Resolves or validates host-derived vSMMU parameters against the physical
    /// SMMU backing an accelerated device.
    ///
    /// Runs once per vSMMU: the first device validates compatibility (TTF,
    /// TTENDIAN, GRAN4K). Before capabilities are frozen, `auto` adopts the host
    /// OAS. Afterward, the advertised OAS is immutable and must not exceed the
    /// host's. `fixed` is always validated as an upper bound. Subsequent devices
    /// must report identical host caps; a mismatch is rejected, since a single
    /// vSMMU cannot be backed by two different physical SMMUs.
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
    fn resolve_host_caps(&self, caps: crate::HostSmmuCaps) -> anyhow::Result<()> {
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

        if caps.ssid_bits > 20 {
            anyhow::bail!(
                "host reported PASID width {} above the PCIe maximum of 20",
                caps.ssid_bits
            );
        }

        if !inner.capabilities_frozen {
            inner.ssid_bits = caps.ssid_bits;
            inner.ats = caps.ats;
        } else {
            if inner.ssid_bits > caps.ssid_bits {
                anyhow::bail!(
                    "advertised SMMU SSID width {} exceeds host PASID width {}",
                    inner.ssid_bits,
                    caps.ssid_bits
                );
            }
            if inner.ats && !caps.ats {
                anyhow::bail!("advertised SMMU ATS is not supported by the host device");
            }
        }

        // OAS: decode the host's IDR5.OAS encoding (may be a reserved value).
        // Before the device starts, `auto` adopts the host value. Once
        // capabilities are guest-visible, both policies only validate the
        // already-advertised value.
        let host_oas_bits = caps.oas.addr_bits().ok_or_else(|| {
            anyhow::anyhow!(
                "host SMMU reported an unknown OAS encoding ({})",
                caps.oas.0
            )
        })?;
        match self.oas_policy {
            crate::SmmuOasPolicy::Auto { .. } if !inner.capabilities_frozen => {
                inner.oas_bits = host_oas_bits;
                inner.oas_mask = (1u64 << host_oas_bits) - 1;
            }
            crate::SmmuOasPolicy::Auto { .. } => {
                if inner.oas_bits > host_oas_bits {
                    anyhow::bail!(
                        "advertised SMMU OAS {} exceeds host SMMU OAS {host_oas_bits}",
                        inner.oas_bits
                    );
                }
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

    /// Updates the stored `GBPA.ABORT` disabled-state policy.
    ///
    /// While the SMMU is enabled this records future policy without changing
    /// current stream attachments. Effective disabled-policy transitions use
    /// [`transition_translation_policy`](Self::transition_translation_policy)
    /// so host attachments change before this value is published.
    pub(crate) fn set_gbpa_abort(&self, abort: bool) {
        self.inner.write().gbpa_abort = abort;
    }

    /// Snapshots the currently effective translation policy.
    pub(crate) fn translation_policy(&self) -> TranslationPolicy {
        let inner = self.inner.read();
        TranslationPolicy {
            enabled: inner.enabled,
            gbpa_abort: inner.gbpa_abort,
            strtab_base: inner.strtab_base,
            strtab_log2size: inner.strtab_log2size,
            oas_mask: inner.oas_mask,
            ssid_bits: inner.ssid_bits,
            ats: inner.ats,
        }
    }

    fn publish_translation_policy(&self, policy: TranslationPolicy) {
        let mut inner = self.inner.write();
        inner.enabled = policy.enabled;
        inner.gbpa_abort = policy.gbpa_abort;
        inner.strtab_base = policy.strtab_base;
        inner.strtab_log2size = policy.strtab_log2size;
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
        let mask = evtq_index_mask(log2size);
        let prod = qs.evtq_prod.wr() & mask;
        let cons = qs.evtq_cons.rd() & mask;
        qs.evtq_prod.set_wr(prod);
        qs.evtq_cons.set_rd(cons);
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

    /// Enters the Event queue overflow condition (§7.4), recording that one or
    /// more event records were discarded.
    ///
    /// The condition is present while `EVENTQ_PROD.OVFLG != EVENTQ_CONS`.`OVACKFLG`,
    /// and the SMMU only toggles OVFLG when the two agree — a second overflow
    /// cannot be indicated before software acknowledges the first.
    fn signal_evtq_overflow(&self, qs: &mut QueueErrorState) {
        if qs.evtq_prod.ovflg() == qs.evtq_cons.ovackflg() {
            qs.evtq_prod.set_ovflg(!qs.evtq_prod.ovflg());
        }
    }

    /// Activates GERROR.EVENTQ_ABT_ERR after an external abort accessing the
    /// Event queue (§7.2.2).
    ///
    /// Per the toggle protocol the bit is set to the inverse of GERRORN's, so
    /// this is a no-op while the error is already unacknowledged.
    fn signal_evtq_abt_err(&self, qs: &mut QueueErrorState) {
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
        let cons = registers::EventqCons::from(cons);
        qs.evtq_cons = registers::EventqCons::new()
            .with_rd(cons.rd() & evtq_index_mask(qs.evtq_log2size))
            .with_ovackflg(cons.ovackflg());
        // Deassert EVTQ IRQ when the guest has drained all events.
        if qs.evtq_irqen && evtq_indices_equal(&qs) {
            if let Some(irq) = &self.evtq_irq {
                irq.set_level(false);
            }
        }
    }

    /// Returns the current event queue producer index (for guest reads
    /// of EVENTQ_PROD on page 1).
    pub(crate) fn evtq_prod(&self) -> registers::EventqProd {
        self.queue_state.lock().evtq_prod
    }

    /// Returns the current event queue consumer index (for guest reads
    /// of EVENTQ_CONS on page 1).
    pub(crate) fn evtq_cons(&self) -> registers::EventqCons {
        self.queue_state.lock().evtq_cons
    }

    /// Initializes the Event queue producer register while the queue is
    /// disabled, as required before enabling or reinitializing it.
    pub(crate) fn set_evtq_prod(&self, prod: u32) {
        let mut qs = self.queue_state.lock();
        let prod = registers::EventqProd::from(prod);
        qs.evtq_prod = registers::EventqProd::new()
            .with_wr(prod.wr() & evtq_index_mask(qs.evtq_log2size))
            .with_ovflg(prod.ovflg());
    }

    /// Resets event queue and GERROR state (called on device reset).
    pub(crate) fn reset_queue_state(&self) {
        let mut qs = self.queue_state.lock();
        qs.evtq_base_addr = 0;
        qs.evtq_log2size = 0;
        qs.evtq_enabled = false;
        qs.evtq_irqen = false;
        qs.evtq_prod = registers::EventqProd::new();
        qs.evtq_cons = registers::EventqCons::new();
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
            evtq_prod: evtq_prod.into(),
            evtq_cons: evtq_cons.into(),
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
        qs.evtq_prod = registers::EventqProd::from(evtq_prod);
        qs.evtq_cons = registers::EventqCons::from(evtq_cons);
        qs.gerror = registers::Gerror::from(gerror);
        qs.gerrorn = registers::Gerror::from(gerrorn);
        self.update_gerror_irq(&qs);
        // Sync EVTQ wired interrupt line to match restored queue state.
        if qs.evtq_irqen {
            if let Some(irq) = &self.evtq_irq {
                irq.set_level(!evtq_indices_equal(&qs));
            }
        }
    }

    /// Registers `backend` as the accelerated stream for `sid` and applies that
    /// stream's current policy.
    ///
    /// Fails only if `sid` is already registered: a StreamID names exactly one
    /// device, because the host keys its vDevice table by it. A guest
    /// renumbering buses can transiently alias two devices, and the loser stays
    /// unregistered — and so blocked — until the winner moves off.
    ///
    /// A backend that rejects its initial policy is left aborting and the error
    /// is logged rather than returned: the registration is still valid, and the
    /// guest's next `CFGI_STE` for this SID retries it. That matches the global
    /// policy transitions, which likewise cannot report a policy failure to the
    /// guest.
    ///
    /// The returned guard unregisters on drop; hold it for as long as the
    /// device keeps this StreamID.
    pub fn register_accel_device(
        self: &Arc<Self>,
        sid: u32,
        backend: Arc<dyn AcceleratedStreamBackend>,
    ) -> anyhow::Result<AccelRegistration> {
        let mut accel = self.accel_state.lock();
        anyhow::ensure!(
            !accel.devices.contains_key(&sid),
            "StreamID {sid:#x} is already registered to another device"
        );

        let config = self.current_stream_config(sid);
        let mut reg = AccelDeviceRegistration {
            backend,
            translating: false,
        };
        if let Err(error) = Self::apply_config_or_abort(sid, &mut reg, config) {
            tracelimit::warn_ratelimited!(
                error = error.as_ref() as &dyn std::error::Error,
                sid,
                "smmu: stream left aborting after failed policy apply at registration"
            );
        }
        accel.devices.insert(sid, reg);

        Ok(AccelRegistration {
            shared: Arc::downgrade(self),
            sid,
        })
    }

    /// Removes the registration for `sid` synchronously, driving its stream to
    /// abort first.
    ///
    /// DMA stops before the StreamID is released, and both happen under the one
    /// lock every config and invalidation path takes, so no `CFGI_STE` can
    /// re-attach the stream behind us and no in-flight invalidation batch can
    /// name a vDevice the caller is about to destroy.
    fn unregister_accel_device(&self, sid: u32) {
        let reg = {
            let mut accel = self.accel_state.lock();
            let Some(reg) = accel.devices.remove(&sid) else {
                return;
            };
            reg.backend
                .set_stream_config(StreamConfig::Abort)
                .unwrap_or_else(|error| {
                    panic!("smmu: cannot stop DMA for SID {sid:#x}: {error:#}")
                });
            reg
        };
        // Released outside the lock so a teardown cannot stall an invalidation.
        drop(reg);
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
        self.stream_config_for_policy(self.translation_policy(), sid)
    }

    /// Computes one stream's host attachment from an explicit policy snapshot.
    pub(crate) fn stream_config_for_policy(
        &self,
        policy: TranslationPolicy,
        sid: u32,
    ) -> StreamConfig {
        if !policy.enabled {
            return if policy.gbpa_abort {
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
            policy.strtab_base,
            policy.strtab_log2size,
            sid,
            policy.oas_mask,
        ) else {
            // Invalid STE (V=0), out-of-range SID, or STE fetch failure.
            return StreamConfig::Abort;
        };

        match translate::ste_config_action(&ste) {
            translate::SteAction::Bypass => StreamConfig::Bypass,
            translate::SteAction::S1Translate => StreamConfig::Translate {
                ste_dwords: canonical_s1_ste_dwords(
                    &ste,
                    policy.oas_mask,
                    policy.ssid_bits,
                    policy.ats,
                ),
            },
            // Config[2]==0 (0b000 / reserved) aborts with no event; an illegal
            // config (0b110/0b111 on this stage-1-only SMMU) also aborts here —
            // its C_BAD_STE, being a data-plane fault, is delivered elsewhere.
            translate::SteAction::Abort | translate::SteAction::Illegal => StreamConfig::Abort,
        }
    }

    fn apply_config(reg: &mut AccelDeviceRegistration, config: StreamConfig) -> anyhow::Result<()> {
        let translating = matches!(config, StreamConfig::Translate { .. });
        // A failed atomic replacement leaves the previous host attachment
        // active, so update forwarding state only after backend success.
        reg.backend.set_stream_config(config)?;
        reg.translating = translating;
        Ok(())
    }

    /// Applies `config`, falling back to [`StreamConfig::Abort`] if the backend
    /// rejects it, and returns the original error so the caller can report it.
    ///
    /// A rejected config leaves the device on its previous attachment, which
    /// may still be translating through structures the guest is about to
    /// reclaim, so the stream must be driven somewhere safe. Abort is the only
    /// such destination; failing to reach it means DMA cannot be stopped at
    /// all, and nothing in the architecture can express that.
    fn apply_config_or_abort(
        sid: u32,
        reg: &mut AccelDeviceRegistration,
        config: StreamConfig,
    ) -> anyhow::Result<()> {
        let error = match Self::apply_config(reg, config) {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };

        if matches!(config, StreamConfig::Abort) {
            panic!("smmu: cannot stop DMA for SID {sid:#x}: {error:#}");
        }
        Self::apply_config(reg, StreamConfig::Abort).unwrap_or_else(|abort_error| {
            panic!("smmu: cannot stop DMA for SID {sid:#x} after {error:#}: {abort_error:#}")
        });
        Err(error)
    }

    /// Recomputes and applies the current policy for one stream.
    pub(crate) fn apply_stream_config(&self, sid: u32) -> anyhow::Result<()> {
        let config = self.current_stream_config(sid);
        let mut accel = self.accel_state.lock();
        let Some(reg) = accel.devices.get_mut(&sid) else {
            return Ok(());
        };
        Self::apply_config_or_abort(sid, reg, config)
    }

    /// Recomputes and applies the current policy for every registered stream
    /// whose StreamID falls within `sids`.
    pub(crate) fn apply_stream_configs_in_range(
        &self,
        sids: std::ops::RangeInclusive<u64>,
    ) -> anyhow::Result<()> {
        let mut accel = self.accel_state.lock();
        let mut first_error = None;
        for (&sid, reg) in accel.devices.iter_mut() {
            if !sids.contains(&u64::from(sid)) {
                continue;
            }
            let config = self.current_stream_config(sid);
            if let Err(error) = Self::apply_config_or_abort(sid, reg, config) {
                first_error.get_or_insert(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Applies a proposed global policy to every accelerated registration, then
    /// publishes it.
    ///
    /// Always completes: SMMUv3 defines no config-plane error reporting, and a
    /// register update must complete in finite time (§6.3.9), so a stream whose
    /// policy the backend rejects is simply left aborting — indistinguishable
    /// to the guest from a stream with an ILLEGAL STE. The guest's next
    /// `CFGI_STE` for that SID retries it.
    pub(crate) fn transition_translation_policy(
        &self,
        policy: TranslationPolicy,
        transition: &'static str,
    ) {
        let mut accel = self.accel_state.lock();
        for (&sid, reg) in accel.devices.iter_mut() {
            let config = self.stream_config_for_policy(policy, sid);
            if let Err(error) = Self::apply_config_or_abort(sid, reg, config) {
                tracelimit::warn_ratelimited!(
                    error = error.as_ref() as &dyn std::error::Error,
                    transition,
                    sid,
                    "smmu: stream left aborting after failed policy transition"
                );
            }
        }
        self.publish_translation_policy(policy);
    }

    /// Locks the accelerated registration table for the duration of an
    /// invalidation batch.
    pub(crate) fn lock_accel_devices(&self) -> AccelDevices<'_> {
        AccelDevices {
            accel: self.accel_state.lock(),
        }
    }

    #[cfg(test)]
    pub(crate) fn accel_devices_locked(&self) -> bool {
        self.accel_state.try_lock().is_none()
    }

    /// Forwards an accumulated invalidation batch to the host vIOMMU.
    ///
    /// Takes the registration guard rather than locking internally: the SID
    /// checks that admitted these entries and the host call that consumes them
    /// must not be split by a concurrent StreamID rebind, which would retire a
    /// vDevice the batch names and make the host reject an otherwise valid
    /// command. An unreachable vIOMMU means no accelerated devices remain, and
    /// so nothing to invalidate.
    pub(crate) fn invalidate(
        devices: &AccelDevices<'_>,
        entries: &[[u64; 2]],
    ) -> Result<(), usize> {
        match devices.accel.viommu.as_ref().and_then(Weak::upgrade) {
            Some(viommu) => viommu.invalidate(entries),
            None => Ok(()),
        }
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
    /// Called from per-device DMA fault paths, from the emulator's command
    /// processing, and from the host fault forwarding path
    /// ([`record_accel_event`](Self::record_accel_event)).
    ///
    /// Per §7.2.1 the queue is writable only when it is enabled, not full, and
    /// free of an unacknowledged access-abort condition; an event that cannot
    /// be written is discarded. Discarding because the queue is *full*
    /// additionally enters the overflow condition (§7.4) — the other two cases
    /// do not.
    pub(crate) fn write_event(&self, event: EvtEntry) {
        let mut qs = self.queue_state.lock();
        if !qs.evtq_enabled {
            return;
        }
        if qs.gerror.eventq_abt_err() != qs.gerrorn.eventq_abt_err() {
            tracelimit::warn_ratelimited!(
                "smmu: EVTQ access abort unacknowledged, discarding event"
            );
            return;
        }

        let max_entries = 1u32 << qs.evtq_log2size;
        let index_mask = (max_entries << 1) - 1;
        let prod = qs.evtq_prod.wr() & index_mask;
        let cons = qs.evtq_cons.rd() & index_mask;

        // Check if the queue is full. Full when the index bits match but
        // the wrap bit differs: (prod ^ cons) == max_entries.
        if (prod ^ cons) == max_entries {
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
            // A failed queue write is an external abort on the Event queue
            // (§7.2.2). The abort is synchronous here, so PROD is not advanced
            // and every entry below it remains valid.
            self.signal_evtq_abt_err(&mut qs);
            return;
        }

        // Advance EVTQ_PROD, leaving the overflow flag untouched.
        qs.evtq_prod.set_wr((prod + 1) & index_mask);

        // Assert EVTQ wired interrupt — held high while queue is non-empty.
        // Deasserted when the guest drains events via CONS writes.
        if qs.evtq_irqen {
            if let Some(irq) = &self.evtq_irq {
                irq.set_level(true);
            }
        }
    }

    /// Records an event the physical SMMU reported for an accelerated stream.
    ///
    /// `record` is a raw SMMUv3 event record (§7.3): four little-endian
    /// quadwords whose StreamID is already expressed in this SMMU's stream
    /// namespace. Accelerated streams translate in hardware, so their faults
    /// are detected by the physical SMMU rather than by this emulator, and this
    /// is the only path by which they reach the guest.
    pub fn record_accel_event(&self, record: [u64; 4]) {
        let event = EvtEntry::read_from_bytes(record.as_bytes())
            .expect("an SMMUv3 event record is exactly one EvtEntry");
        tracing::debug!(
            event_id = event.header.event_id().0,
            sid = event.sid,
            input_addr = event.input_addr,
            "smmu: host SMMU event for an accelerated stream"
        );
        self.write_event(event);
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
#[error("SMMU DMA fault: event {event_id:#x?} SID {sid:#x} addr {input_addr:#x}")]
pub struct SmmuDmaFault {
    /// Event type ID. Zero signifies that no event record was generated.
    event_id: EventId,
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
    /// (`STE.Config[2]==0`).
    fn no_event_abort(sid: u32, input_addr: u64) -> Self {
        Self {
            event_id: EventId(0),
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
    use crate::spec::AddrSize;
    use crate::spec::cd::CD_SIZE;
    use crate::spec::cd::CdDw0;
    use crate::spec::cd::CdDw1;
    use crate::spec::cd::Tg0;
    use crate::spec::events::EventId;
    use crate::spec::pt::ApBits;
    use crate::spec::pt::PtDesc;
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
    fn test_dma_fault_event_representation() {
        let fault = SmmuDmaFault::from_event(&EvtEntry::translation_fault(0x12, 0x3456, false));
        assert_eq!(fault.event_id, EventId::F_TRANSLATION);
        assert_eq!(
            fault.to_string(),
            "SMMU DMA fault: event F_TRANSLATION SID 0x12 addr 0x3456"
        );

        let abort = SmmuDmaFault::no_event_abort(0x12, 0x3456);
        assert_eq!(abort.event_id, EventId(0));
        assert_eq!(
            abort.to_string(),
            "SMMU DMA fault: event 0x0 SID 0x12 addr 0x3456"
        );
    }

    fn transition_to_enabled(state: &SmmuSharedState) {
        let mut policy = state.translation_policy();
        policy.enabled = true;
        state.transition_translation_policy(policy, "test enable transition");
    }

    /// OAS mask for a 48-bit output address size.
    const TEST_OAS_MASK: u64 = (1 << 48) - 1;

    #[test]
    fn test_canonical_s1_ste_dwords_preserves_allowed_fields() {
        // Set every field the canonical set retains, with distinct values.
        let cd_addr: u64 = 0xFFFF_FFFF_F000;
        let qw0 = SteDw0::new()
            .with_v(true)
            .with_config(SteConfig::S1_TRANS.0)
            .with_s1_context_ptr(cd_addr >> 6);
        let qw1 = SteDw1::new()
            .with_s1_cir(0x3)
            .with_s1_cor(0x3)
            .with_s1_csh(0x3)
            .with_s1stalld(true);
        let ste = Ste {
            qw0,
            qw1,
            _qw2_7: [0; 6],
        };

        let [out0, out1] = canonical_s1_ste_dwords(&ste, TEST_OAS_MASK, 0, false);
        // Retained fields survive untouched.
        assert_eq!(out0, u64::from(qw0));
        assert_eq!(out1, u64::from(qw1));
    }

    #[test]
    fn test_canonical_s1_ste_dwords_drops_ignored_fields() {
        // A fully-populated STE must be reduced to only the retained fields.
        let ste = Ste {
            qw0: SteDw0::from(u64::MAX),
            qw1: SteDw1::from(u64::MAX),
            _qw2_7: [u64::MAX; 6],
        };
        let [out0, out1] = canonical_s1_ste_dwords(&ste, TEST_OAS_MASK, 0, false);

        // DW0 retains V[0] | Config[3:1] | S1ContextPtr[55:6], the pointer
        // truncated to the 48-bit OAS. S1Fmt[5:4] and S1CDMax[63:59] are
        // IGNORED with IDR1.SSIDSIZE == 0, as are the reserved bits [58:56].
        assert_eq!(out0, 0x0000_ffff_ffff_ffcf);
        // DW1 retains S1CIR[3:2] | S1COR[5:4] | S1CSH[7:6] | S1STALLD[27].
        // S1DSS[1:0] is IGNORED with SSIDSIZE == 0 and EATS[29:28] with
        // IDR0.ATS == 0; everything else (STRW, SHCFG, NSCFG, PRIVCFG,
        // stage-2/override fields, ...) is RES0/IGNORED.
        assert_eq!(out1, 0x0800_00fc);
    }

    #[test]
    fn test_canonical_s1_ste_dwords_preserves_pasid_and_ats_fields() {
        let ste = Ste {
            qw0: SteDw0::new()
                .with_v(true)
                .with_config(SteConfig::S1_TRANS.0)
                .with_s1_fmt(2)
                .with_s1_context_ptr(0x1234)
                .with_s1_cd_max(14),
            qw1: SteDw1::new().with_s1_dss(2).with_eats(1),
            _qw2_7: [0; 6],
        };

        let [out0, out1] = canonical_s1_ste_dwords(&ste, TEST_OAS_MASK, 14, true);
        let out0 = SteDw0::from(out0);
        let out1 = SteDw1::from(out1);
        assert_eq!(out0.s1_fmt(), 2);
        assert_eq!(out0.s1_cd_max(), 14);
        assert_eq!(out1.s1_dss(), 2);
        assert_eq!(out1.eats(), 1);
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
                .with_ips(AddrSize::BITS_40)
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
        transition_to_enabled(&state);
        // Enable EVTQ so fault events are written to guest memory.
        state.set_evtq_config(EVTQ_BASE, EVTQ_LOG2SIZE);
        state.set_evtq_enabled(true);
        state
    }

    /// Count events currently queued in the EVTQ as the distance between the
    /// producer and consumer indices. Masking to the index-with-wrap range
    /// keeps the OVFLG handshake bit and producer wrap out of the count.
    fn evtq_event_count(state: &SmmuSharedState) -> u32 {
        let index_mask = evtq_index_mask(EVTQ_LOG2SIZE);
        state.evtq_prod().wr().wrapping_sub(state.evtq_cons().rd()) & index_mask
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
        assert_eq!(written.header.event_id(), EventId::F_TRANSLATION);
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
            oas: AddrSize::BITS_48,
            ttf: registers::Idr0Ttf::new().with_aarch64(true),
            ttendian: registers::Idr0TtEndian::LE,
            gran4k: true,
            ssid_bits: 14,
            ats: true,
        }
    }

    /// An accel-mode shared state with the given OAS policy.
    fn make_accel_state(policy: crate::SmmuOasPolicy) -> Arc<SmmuSharedState> {
        let gm = GuestMemory::allocate(0x1000);
        let oas_bits = match policy {
            crate::SmmuOasPolicy::Auto { provisional } => provisional,
            crate::SmmuOasPolicy::Fixed(bits) => bits,
        };
        SmmuSharedState::new(gm, oas_bits, policy, true, None, None)
    }

    #[test]
    fn resolve_host_caps_accepts_compatible_host() {
        let state = make_accel_state(crate::SmmuOasPolicy::Fixed(40));
        state.resolve_host_caps(compatible_host_caps()).unwrap();
    }

    #[test]
    fn resolve_host_caps_updates_ats_support() {
        let supported = make_accel_state(crate::SmmuOasPolicy::Fixed(40));
        supported.resolve_host_caps(compatible_host_caps()).unwrap();
        assert!(supported.ats_supported());

        let unsupported = make_accel_state(crate::SmmuOasPolicy::Fixed(40));
        unsupported
            .resolve_host_caps(crate::HostSmmuCaps {
                ats: false,
                ..compatible_host_caps()
            })
            .unwrap();
        assert!(!unsupported.ats_supported());
    }

    #[test]
    fn resolve_host_caps_auto_adopts_host_oas() {
        let state = make_accel_state(crate::SmmuOasPolicy::Auto { provisional: 40 });
        let caps = crate::HostSmmuCaps {
            oas: AddrSize::BITS_48,
            ..compatible_host_caps()
        };
        state.resolve_host_caps(caps).unwrap();
        assert_eq!(state.oas_bits(), 48);
    }

    #[test]
    fn resolve_host_caps_auto_after_freeze_preserves_advertised_oas() {
        let state = make_accel_state(crate::SmmuOasPolicy::Auto { provisional: 40 });
        state.freeze_capabilities();

        state.resolve_host_caps(compatible_host_caps()).unwrap();
        assert_eq!(state.oas_bits(), 40);

        // A later device with the same physical-SMMU capabilities is accepted
        // without changing the guest-visible OAS.
        state.resolve_host_caps(compatible_host_caps()).unwrap();
        assert_eq!(state.oas_bits(), 40);
    }

    #[test]
    fn resolve_host_caps_auto_after_freeze_rejects_narrower_host() {
        let state = make_accel_state(crate::SmmuOasPolicy::Auto { provisional: 40 });
        state.freeze_capabilities();
        let caps = crate::HostSmmuCaps {
            oas: AddrSize::BITS_36,
            ..compatible_host_caps()
        };

        let err = state.resolve_host_caps(caps).unwrap_err().to_string();
        assert!(err.contains("advertised SMMU OAS 40 exceeds host SMMU OAS 36"));
        assert_eq!(state.oas_bits(), 40);
    }

    #[test]
    fn resolve_host_caps_rejects_fixed_oas_above_host() {
        let state = make_accel_state(crate::SmmuOasPolicy::Fixed(52));
        let caps = crate::HostSmmuCaps {
            oas: AddrSize::BITS_44,
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
            oas: AddrSize::BITS_44,
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
            StreamConfig::Translate { .. }
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
}
