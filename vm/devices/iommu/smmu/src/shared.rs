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
use pci_core::msi::SignalMsi;
use std::sync::Arc;
use vmcore::irqfd::IrqFd;
use vmcore::irqfd::IrqFdRoute;
use vmcore::line_interrupt::LineInterrupt;
use zerocopy::IntoBytes;

/// Result of an SMMU translation attempt.
#[derive(Debug)]
enum TranslateResult {
    /// SMMU disabled or bus not yet assigned — bypass (IOVA = GPA).
    Bypass,
    /// Translated GPA.
    Translated(u64),
    /// Abort — STE says to abort this stream's DMA.
    Abort(EvtEntry),
    /// Translation fault — event to queue.
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
}

struct SharedStateInner {
    /// Whether the SMMU is enabled (CR0.SMMUEN).
    enabled: bool,
    /// Stream table base address.
    strtab_base: u64,
    /// Stream table log2 size (number of entries).
    strtab_log2size: u8,
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
    /// `oas_bits` is the output address size in bits (e.g., 40 for a 40-bit
    /// physical address space). Computed addresses for STE/CD/PT fetches are
    /// truncated to this width, matching hardware behavior per SMMUv3 §3.4.
    pub fn new(
        guest_memory: GuestMemory,
        oas_bits: u8,
        evtq_irq: Option<LineInterrupt>,
        gerror_irq: Option<LineInterrupt>,
    ) -> Arc<Self> {
        let oas_mask = (1u64 << oas_bits) - 1;
        Arc::new(Self {
            inner: RwLock::new(SharedStateInner {
                enabled: false,
                strtab_base: 0,
                strtab_log2size: 0,
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
        })
    }

    /// Updates the SMMU enable state (called by SmmuDevice on CR0 writes).
    pub fn set_enabled(&self, enabled: bool) {
        self.inner.write().enabled = enabled;
    }

    /// Updates the stream table configuration (called by SmmuDevice on
    /// STRTAB_BASE / STRTAB_BASE_CFG writes).
    pub fn set_strtab(&self, base: u64, log2size: u8) {
        let mut inner = self.inner.write();
        inner.strtab_base = base;
        inner.strtab_log2size = log2size;
    }

    /// Updates the event queue configuration (called by SmmuDevice on
    /// EVTQ_BASE writes).
    pub fn set_evtq_config(&self, base_addr: u64, log2size: u8) {
        let mut qs = self.queue_state.lock();
        qs.evtq_base_addr = base_addr;
        qs.evtq_log2size = log2size;
    }

    /// Updates the event queue enabled state (called on CR0 writes).
    pub fn set_evtq_enabled(&self, enabled: bool) {
        self.queue_state.lock().evtq_enabled = enabled;
    }

    /// Updates both interrupt enable flags from IRQ_CTRL (called on
    /// IRQ_CTRL writes). Also updates the GERROR interrupt line level.
    pub fn set_irq_ctrl(&self, evtq_irqen: bool, gerror_irqen: bool) {
        let mut qs = self.queue_state.lock();
        qs.evtq_irqen = evtq_irqen;
        qs.gerror_irqen = gerror_irqen;
        self.update_gerror_irq(&qs);
    }

    /// Reads the current GERROR register value.
    pub fn read_gerror(&self) -> registers::Gerror {
        self.queue_state.lock().gerror
    }

    /// Reads the current GERRORN register value.
    pub fn read_gerrorn(&self) -> registers::Gerror {
        self.queue_state.lock().gerrorn
    }

    /// Returns true if GERROR.CMDQ_ERR != GERRORN.CMDQ_ERR (error active).
    pub fn cmdq_err_active(&self) -> bool {
        let qs = self.queue_state.lock();
        qs.gerror.cmdq_err() != qs.gerrorn.cmdq_err()
    }

    /// Writes GERRORN (guest acknowledging errors) and updates the
    /// interrupt line level.
    pub fn write_gerrorn(&self, value: u32) {
        let mut qs = self.queue_state.lock();
        qs.gerrorn = registers::Gerror::from(value);
        self.update_gerror_irq(&qs);
    }

    /// Toggles GERROR.CMDQ_ERR to signal a command queue error.
    ///
    /// Updates the interrupt line level under the lock.
    pub fn toggle_cmdq_err(&self) {
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
    pub fn set_evtq_cons(&self, cons: u32) {
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
    pub fn evtq_prod(&self) -> u32 {
        self.queue_state.lock().evtq_prod
    }

    /// Returns the current event queue consumer index (for guest reads
    /// of EVENTQ_CONS on page 1).
    pub fn evtq_cons(&self) -> u32 {
        self.queue_state.lock().evtq_cons
    }

    /// Resets event queue and GERROR state (called on device reset).
    pub fn reset_queue_state(&self) {
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
        let action = match translate::ste_config_action(&ste) {
            Ok(action) => action,
            Err(_) => return TranslateResult::Fault(EvtEntry::bad_ste(sid)),
        };

        match action {
            translate::SteAction::Abort => TranslateResult::Abort(EvtEntry::bad_ste(sid)),
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
    pub fn write_event(&self, event: EvtEntry) {
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
            TranslateResult::Abort(event) | TranslateResult::Fault(event) => {
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
            TranslateResult::Abort(event) => {
                self.shared.write_event(event);
                tracelimit::warn_ratelimited!(sid, address, "smmu: MSI aborted by STE config");
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
            TranslateResult::Abort(event) => {
                self.shared.write_event(event);
                tracelimit::warn_ratelimited!(
                    sid,
                    address,
                    "smmu: irqfd MSI route aborted by STE config"
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
        let state = SmmuSharedState::new(gm.clone(), 40, None, None);
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

        // STE in abort mode.
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

        // Should have written an event to the EVTQ.
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

        let state = SmmuSharedState::new(gm.clone(), 40, None, None);
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
}
