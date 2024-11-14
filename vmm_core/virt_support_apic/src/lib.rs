// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Local APIC emulator.
//!
//! This emulates the local APIC, as documented by the Intel SDM. It supports
//! both legacy (MMIO) and X2APIC (MSR) modes.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

use bitfield_struct::bitfield;
use inspect::Inspect;
use inspect_counters::Counter;
use parking_lot::RwLock;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use virt::x86::vp::ApicRegisters;
use virt::x86::MsrError;
use vm_topology::processor::x86::X86VpInfo;
use vm_topology::processor::VpIndex;
use vmcore::vmtime::VmTime;
use vmcore::vmtime::VmTimeAccess;
use x86defs::apic::ApicBase;
use x86defs::apic::ApicRegister;
use x86defs::apic::Dcr;
use x86defs::apic::DeliveryMode;
use x86defs::apic::DestinationShorthand;
use x86defs::apic::Dfr;
use x86defs::apic::Icr;
use x86defs::apic::Lvt;
use x86defs::apic::Svr;
use x86defs::apic::TimerMode;
use x86defs::apic::X2ApicLogicalId;
use x86defs::apic::XApicClusterLogicalId;
use x86defs::apic::APIC_BASE_PAGE;
use x86defs::apic::X2APIC_MSR_BASE;
use x86defs::apic::X2APIC_MSR_END;
use x86defs::msi::MsiAddress;
use x86defs::msi::MsiData;
use x86defs::X86X_MSR_APIC_BASE;

const NANOS_PER_TICK: u64 = 5; // 200Mhz
const TIMER_FREQUENCY: u64 = 1_000_000_000 / NANOS_PER_TICK;
const APIC_VERSION: u32 = 0x50014;

const ICR_LOW_MASK: Icr = Icr::new()
    .with_vector(!0)
    .with_delivery_mode(0b111)
    .with_destination_mode_logical(true)
    .with_level_assert(true)
    .with_trigger_mode_level(true)
    .with_destination_shorthand(0b11);

const ICR_XAPIC_MASK: Icr = ICR_LOW_MASK.with_xapic_mda(!0);
const ICR_X2APIC_MASK: Icr = ICR_LOW_MASK.with_x2apic_mda(!0);

/// An individual local APIC for a processor.
#[derive(Inspect)]
pub struct LocalApic {
    #[inspect(flatten)]
    shared: Arc<SharedState>,
    #[inspect(skip)]
    global: Arc<GlobalState>,

    #[inspect(hex)]
    apic_base: u64,
    #[inspect(hex)]
    base_address: Option<u64>,
    #[inspect(hex)]
    id: u32,
    #[inspect(hex)]
    version: u32,
    #[inspect(hex)]
    ldr: u32,
    cluster_mode: bool,
    #[inspect(hex)]
    svr: u32,
    #[inspect(with = "|x| inspect::iter_by_index(x.to_bits()).map_value(inspect::AsHex)")]
    isr: IsrStack,
    #[inspect(with = "|x| inspect::iter_by_index(x).map_value(inspect::AsHex)")]
    irr: [u32; 8],
    #[inspect(with = "|x| inspect::iter_by_index(x).map_value(inspect::AsHex)")]
    tmr: [u32; 8],
    #[inspect(with = "|x| inspect::iter_by_index(x).map_value(inspect::AsHex)")]
    auto_eoi: [u32; 8],
    next_irr: Option<u8>,
    #[inspect(hex)]
    esr: u32,
    #[inspect(hex)]
    icr: u64,
    #[inspect(hex)]
    lvt_timer: u32,
    #[inspect(hex)]
    lvt_thermal: u32,
    #[inspect(hex)]
    lvt_pmc: u32,
    #[inspect(with = "|x| inspect::iter_by_index(x).map_value(inspect::AsHex)")]
    lvt_lint: [u32; 2],
    #[inspect(hex)]
    lvt_error: u32,
    #[inspect(hex)]
    timer_icr: u32,
    #[inspect(hex)]
    timer_ccr: u32,
    last_time: VmTime,
    next_timeout: Option<VmTime>,
    #[inspect(hex)]
    timer_dcr: u32,
    active_auto_eoi: bool,
    is_offloaded: bool,
    needs_offload_reeval: bool,

    stats: Stats,
}

#[derive(Inspect, Default)]
struct Stats {
    eoi: Counter,
    eoi_level: Counter,
    spurious_eoi: Counter,
    lazy_eoi: Counter,
    interrupt: Counter,
    nmi: Counter,
    extint: Counter,
    init: Counter,
    sipi: Counter,
    self_ipi: Counter,
    broadcast_ipi: Counter,
    other_ipi: Counter,
    offload_push: Counter,
    offload_pull: Counter,
}

fn priority(v: u8) -> u8 {
    v >> 4
}

fn dcr_divider_shift(dcr: Dcr) -> u8 {
    let value = dcr.value_low() | (dcr.value_high() << 2);
    value.wrapping_add(1) & 0b111
}

fn bank_mask(vector: u8) -> (usize, u32) {
    (vector as usize / 32, 1 << (vector % 32))
}

fn cluster_mode(value: u32) -> bool {
    match Dfr(value | 0x0fff_ffff) {
        Dfr::CLUSTERED_MODE => true,
        Dfr::FLAT_MODE => false,
        _ => unreachable!("Unknown DFR value {value}"),
    }
}

#[derive(Debug)]
struct IsrStack(Vec<u8>);

impl IsrStack {
    fn new() -> Self {
        Self(Vec::with_capacity(16))
    }

    fn push(&mut self, v: u8) {
        assert!(v >= 16);
        assert!(self.0.len() < 16);
        assert!(priority(self.top().unwrap_or(0)) < priority(v));

        self.0.push(v);
    }

    fn to_bits(&self) -> [u32; 8] {
        let mut bits = [0; 8];
        for &v in &self.0 {
            bits[v as usize / 32] |= 1 << (v % 32);
        }
        bits
    }

    fn load_from_bits(&mut self, bits: [u32; 8]) {
        // Only restore at most one interrupt per priority level, skipping the
        // first (invalid) level.
        self.clear();
        let bits = bits.map(|v| [v as u16, (v >> 16) as u16]);
        for (pri, &v) in bits.iter().flatten().enumerate().skip(1) {
            if v != 0 {
                let n = 15 - v.leading_zeros() as u8;
                self.push(pri as u8 * 16 + n);
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn top(&self) -> Option<u8> {
        self.0.last().copied()
    }

    fn pop(&mut self) -> Option<u8> {
        self.0.pop()
    }

    fn clear(&mut self) {
        self.0.clear();
    }
}

#[derive(Debug, Inspect)]
struct SharedState {
    vp_index: VpIndex,
    #[inspect(
        with = "|x| inspect::iter_by_index(x).map_value(|x| inspect::AsHex(x.load(Ordering::Relaxed)))"
    )]
    tmr: [AtomicU32; 8],
    #[inspect(
        with = "|x| inspect::iter_by_index(x).map_value(|x| inspect::AsHex(x.load(Ordering::Relaxed)))"
    )]
    new_irr: [AtomicU32; 8],
    #[inspect(
        with = "|x| inspect::iter_by_index(x).map_value(|x| inspect::AsHex(x.load(Ordering::Relaxed)))"
    )]
    auto_eoi: [AtomicU32; 8],
    work: AtomicU32,
}

#[bitfield(u32)]
struct WorkFlags {
    init: bool,
    sipi: bool,
    sipi_vector: u8,
    extint: bool,
    nmi: bool,
    #[bits(20)]
    _rsvd: u32,
}

/// The interface to the local APIC for all processors.
#[derive(Inspect)]
pub struct LocalApicSet {
    #[inspect(flatten)]
    global: Arc<GlobalState>,
}

#[derive(Debug, Inspect)]
struct GlobalState {
    x2apic_capable: bool,
    hyperv_enlightenments: bool,
    #[inspect(flatten)]
    mutable: RwLock<MutableGlobalState>,
}

#[derive(Debug, Inspect)]
struct MutableGlobalState {
    x2apic_enabled: usize,
    logical_cluster_mode: usize,
    #[inspect(
        with = "|x| inspect::iter_by_key(x.iter().enumerate().filter(|x| x.1.shared.is_some()))"
    )]
    by_apic_id: Vec<ApicSlot>,
    #[inspect(iter_by_index)]
    by_index: Vec<u32>,
}

#[derive(Debug, Inspect)]
struct ApicSlot {
    logical_id: u8,
    hardware_enabled: bool,
    software_enabled: bool,
    cluster_mode: bool,
    x2apic_enabled: bool,
    #[inspect(skip)]
    lint: [Lvt; 2],
    #[inspect(skip)]
    shared: Option<Arc<SharedState>>,
}

/// Builder for [`LocalApicSet`].
pub struct LocalApicSetBuilder {
    /// Allow X2APIC mode.
    x2apic_capable: bool,
    /// Handle Hyper-V enlightenment MSRs.
    hyperv_enlightenments: bool,
}

impl LocalApicSetBuilder {
    fn new() -> Self {
        Self {
            x2apic_capable: false,
            hyperv_enlightenments: false,
        }
    }

    /// Sets whether X2APIC mode is allowed.
    pub fn x2apic_capable(&mut self, x2apic_capable: bool) -> &mut Self {
        self.x2apic_capable = x2apic_capable;
        self
    }

    /// Sets whether Hyper-V enlightenment MSRs are handled.
    pub fn hyperv_enlightenments(&mut self, hyperv_enlightenments: bool) -> &mut Self {
        self.hyperv_enlightenments = hyperv_enlightenments;
        self
    }

    /// Builds a new local APIC set.
    pub fn build(&self) -> LocalApicSet {
        LocalApicSet {
            global: Arc::new(GlobalState {
                x2apic_capable: self.x2apic_capable,
                hyperv_enlightenments: self.hyperv_enlightenments,
                mutable: RwLock::new(MutableGlobalState {
                    x2apic_enabled: 0,
                    logical_cluster_mode: 0,
                    by_apic_id: Vec::new(),
                    by_index: Vec::new(),
                }),
            }),
        }
    }
}

impl LocalApicSet {
    /// Creates a new builder for a local APIC set.
    pub fn builder() -> LocalApicSetBuilder {
        LocalApicSetBuilder::new()
    }

    /// Returns the frequency of the APIC timer clock.
    pub fn frequency(&self) -> u64 {
        TIMER_FREQUENCY
    }

    /// Adds an APIC for the specified VP to the set.
    pub fn add_apic(&self, vp: &X86VpInfo) -> LocalApic {
        let shared = Arc::new(SharedState {
            vp_index: vp.base.vp_index,
            tmr: Default::default(),
            new_irr: Default::default(),
            auto_eoi: Default::default(),
            work: 0.into(),
        });

        {
            let mut mutable = self.global.mutable.write();
            if mutable.by_apic_id.len() <= vp.apic_id as usize {
                mutable
                    .by_apic_id
                    .resize_with(vp.apic_id as usize + 1, || ApicSlot {
                        logical_id: 0,
                        hardware_enabled: false,
                        software_enabled: false,
                        cluster_mode: false,
                        x2apic_enabled: false,
                        lint: [Lvt::new(); 2],
                        shared: None,
                    });
            }
            assert!(mutable.by_apic_id[vp.apic_id as usize].shared.is_none());
            mutable.by_apic_id[vp.apic_id as usize].shared = Some(shared.clone());
            if mutable.by_index.len() <= vp.base.vp_index.index() as usize {
                mutable
                    .by_index
                    .resize(vp.base.vp_index.index() as usize + 1, !0);
            }
            mutable.by_index[vp.base.vp_index.index() as usize] = vp.apic_id;
        }

        let mut apic = LocalApic {
            shared,
            global: self.global.clone(),
            apic_base: 0,
            base_address: None,
            id: vp.apic_id,
            version: APIC_VERSION,
            ldr: 0,
            cluster_mode: false,
            svr: 0,
            isr: IsrStack::new(),
            next_irr: None,
            irr: [0; 8],
            tmr: [0; 8],
            auto_eoi: [0; 8],
            esr: 0,
            icr: 0,
            lvt_timer: 0,
            lvt_thermal: 0,
            lvt_pmc: 0,
            lvt_lint: [0; 2],
            lvt_error: 0,
            timer_icr: 0,
            timer_ccr: 0,
            timer_dcr: 0,
            last_time: VmTime::from_100ns(0),
            next_timeout: None,
            active_auto_eoi: false,
            needs_offload_reeval: false,
            is_offloaded: false,
            stats: Stats::default(),
        };
        apic.reset();
        apic
    }

    /// Requests a message-signaled interrupt.
    ///
    /// Calls `wake` for each processor that should be woken up for APIC
    /// handling.
    pub fn request_interrupt(&self, address: u64, data: u32, wake: impl FnMut(VpIndex)) {
        let address = MsiAddress::from(address as u32);
        let data = MsiData::from(data);
        self.global.request_interrupt(
            Destination::from_external(
                address.destination_mode_logical(),
                address.virt_destination().into(),
                self.global.x2apic_capable,
            ),
            DeliveryMode(data.delivery_mode()),
            data.vector(),
            data.trigger_mode_level(),
            wake,
        );
    }

    /// Pulses the specified LINT.
    ///
    /// Typically LINT0 is programmed by the guest for EXTINT interrupts and
    /// LINT1 is programmed for NMIs.
    pub fn lint(&self, vp_index: VpIndex, lint_index: usize, wake: impl FnOnce(VpIndex)) {
        let mutable = self.global.mutable.read();
        if let Some(slot) = mutable
            .by_index
            .get(vp_index.index() as usize)
            .and_then(|&apic_id| mutable.by_apic_id.get(apic_id as usize))
        {
            let lvt = slot.lint[lint_index];
            if !lvt.masked() {
                if lvt.trigger_mode_level() {
                    // Don't know how to manage remote IRR.
                    return;
                }
                slot.request_interrupt(
                    DeliveryMode(lvt.delivery_mode()),
                    lvt.vector(),
                    lvt.trigger_mode_level(),
                    false,
                    wake,
                );
            }
        }
    }

    /// Asserts a synic interrupt to the specified virtual processor, optionally
    /// with auto EOI (meaning the corresponding ISR bit will not be set when
    /// the interrupt is delivered).
    pub fn synic_interrupt(
        &self,
        vp_index: VpIndex,
        vector: u8,
        auto_eoi: bool,
        wake: impl FnOnce(VpIndex),
    ) {
        let mutable = self.global.mutable.read();
        if let Some(slot) = mutable
            .by_index
            .get(vp_index.index() as usize)
            .and_then(|&apic_id| mutable.by_apic_id.get(apic_id as usize))
        {
            slot.request_interrupt(DeliveryMode::FIXED, vector, false, auto_eoi, wake);
        }
    }
}

impl GlobalState {
    fn request_interrupt(
        &self,
        destination: Destination,
        delivery_mode: DeliveryMode,
        vector: u8,
        level: bool,
        mut wake: impl FnMut(VpIndex),
    ) {
        let mutable = self.mutable.read();
        match destination {
            Destination::Physical(id) => {
                if let Some(slot) = mutable.by_apic_id.get(id as usize) {
                    slot.request_interrupt(delivery_mode, vector, level, false, &mut wake);
                }
            }
            Destination::Logical(id) => {
                if mutable.x2apic_enabled > 0 {
                    // X2APIC cluster mode.
                    if id == !0 {
                        mutable.request_broadcast_interrupt(
                            delivery_mode,
                            vector,
                            level,
                            &mut wake,
                        );
                    } else {
                        let lowest_priority = delivery_mode == DeliveryMode::LOWEST_PRIORITY;
                        let id = X2ApicLogicalId::from(id);
                        let base = (id.cluster_id() as u32) << 4;
                        for i in 0..16 {
                            if id.logical_id() & (1 << i) == 0 {
                                continue;
                            }
                            let phys_id = base | i;
                            if let Some(slot) = mutable.by_apic_id.get(phys_id as usize) {
                                // For now, just pick the first enabled APIC in the set for lowest priority.
                                if !lowest_priority || slot.software_enabled {
                                    slot.request_interrupt(
                                        delivery_mode,
                                        vector,
                                        level,
                                        false,
                                        &mut wake,
                                    );
                                    if lowest_priority {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                } else if mutable.logical_cluster_mode > 0 {
                    if id as u8 == !0 {
                        mutable.request_broadcast_interrupt(
                            delivery_mode,
                            vector,
                            level,
                            &mut wake,
                        );
                    } else {
                        // XAPIC cluster mode. Easy and fast to iterate through the APICs.
                        let id = XApicClusterLogicalId::from(id as u8);
                        mutable.request_set_interrupt(
                            delivery_mode,
                            vector,
                            level,
                            &mut wake,
                            |_, slot| {
                                let ldr = XApicClusterLogicalId::from(slot.logical_id);
                                ldr.cluster_id() == id.cluster_id()
                                    && ldr.logical_id() & id.logical_id() != 0
                            },
                        );
                    }
                } else {
                    // APIC flat mode. Just iterate through all the VPs.
                    mutable.request_set_interrupt(
                        delivery_mode,
                        vector,
                        level,
                        &mut wake,
                        |_, slot| slot.logical_id & id as u8 != 0,
                    );
                }
            }
            Destination::Broadcast => {
                mutable.request_broadcast_interrupt(delivery_mode, vector, level, &mut wake);
            }
            Destination::AllExcept(except) => {
                mutable.request_set_interrupt(
                    delivery_mode,
                    vector,
                    level,
                    &mut wake,
                    |apic_id, _| apic_id != except,
                );
            }
        }
    }
}

enum Destination {
    Physical(u32),
    Logical(u32),
    Broadcast,
    AllExcept(u32),
}

impl Destination {
    fn from_icr(icr: Icr, x2apic: bool) -> Self {
        if x2apic {
            if icr.destination_mode_logical() {
                Self::Logical(icr.x2apic_mda())
            } else if icr.x2apic_mda() == !0u32 {
                Self::Broadcast
            } else {
                Self::Physical(icr.x2apic_mda())
            }
        } else {
            if icr.destination_mode_logical() {
                Self::Logical(icr.xapic_mda().into())
            } else if icr.xapic_mda() == !0u8 {
                Self::Broadcast
            } else {
                Self::Physical(icr.xapic_mda().into())
            }
        }
    }

    fn from_external(
        logical_destination_mode: bool,
        destination: u32,
        x2apic_capable: bool,
    ) -> Self {
        if logical_destination_mode {
            Self::Logical(destination)
        } else if (x2apic_capable && destination == !0u32)
            || (!x2apic_capable && destination == 0xff)
        {
            Self::Broadcast
        } else {
            Self::Physical(destination)
        }
    }
}

/// Access to a local APIC.
pub struct LocalApicAccess<'a, T> {
    apic: &'a mut LocalApic,
    client: &'a mut T,
}

/// The client to pass to [`LocalApic::access`], to handle requests needed when
/// accessing the APIC.
pub trait ApicClient {
    /// Get the CR8 register.
    fn cr8(&mut self) -> u32;

    /// Set the CR8 register.
    fn set_cr8(&mut self, value: u32);

    /// Set the APIC base MSR.
    ///
    /// This is just to accelerate reads of the MSR. If apic base MSR reads
    /// always come to `msr_read`, then this can be a no-op.
    fn set_apic_base(&mut self, value: u64);

    /// Ensure the processor at `vp_index` calls `scan` soon.
    fn wake(&mut self, vp_index: VpIndex);

    /// Notify the IO-APIC of an EOI.
    fn eoi(&mut self, vector: u8);

    /// Returns the current time.
    fn now(&mut self) -> VmTime;

    /// Retrieve the offloaded IRR and ISR state, clearing them in the
    /// offloaded APIC.
    fn pull_offload(&mut self) -> ([u32; 8], [u32; 8]);
}

fn is_valid_apic_access(address: u64) -> bool {
    // Any aligned access is valid.
    if address & 0xf == 0 {
        return true;
    }
    // Allow high byte accesses for some registers. This isn't spec compliant
    // but some guests rely on this.
    if address & 0xf == 3 {
        return matches!(
            ApicRegister((address >> 4) as u8),
            ApicRegister::ID | ApicRegister::LDR | ApicRegister::DFR
        );
    }
    false
}

impl<T: ApicClient> LocalApicAccess<'_, T> {
    /// Performs an EOI that was signaled lazily, out of band from the normal
    /// APIC interfaces.
    pub fn lazy_eoi(&mut self) {
        debug_assert!(self.apic.is_lazy_eoi_pending());
        self.eoi(true);
    }

    fn eoi(&mut self, lazy: bool) {
        self.ensure_state_local();
        if let Some(vector) = self.apic.isr.pop() {
            tracing::trace!(vector, "eoi");
            if lazy {
                self.apic.stats.lazy_eoi.increment();
            } else {
                self.apic.stats.eoi.increment();
            }
            let (bank, mask) = bank_mask(vector);
            // If this was a level-triggered interrupt, notify IO-APIC of the EOI.
            if self.apic.tmr[bank] & mask != 0 {
                self.client.eoi(vector);
                self.apic.stats.eoi_level.increment();
            }
        } else {
            tracelimit::warn_ratelimited!(lazy, "eoi when no interrupts pending");
            self.apic.stats.spurious_eoi.increment();
        }
    }

    /// Reads from the legacy APIC MMIO page.
    pub fn mmio_read(&mut self, address: u64, data: &mut [u8]) {
        if !self.apic.xapic_enabled() || !is_valid_apic_access(address) {
            tracelimit::warn_ratelimited!(
                address,
                len = data.len(),
                enabled = self.apic.hardware_enabled(),
                x2apic = self.apic.x2apic_enabled(),
                "invalid apic read"
            );
            data.fill(!0);
            return;
        }

        let value = self
            .read_register(ApicRegister((address >> 4) as u8))
            .unwrap_or(0);

        let offset = address as usize & 3;
        data.fill(0);
        let len = data.len().min(4 - offset);
        let data = &mut data[..len];
        data.copy_from_slice(&value.to_ne_bytes()[offset..offset + data.len()]);
    }

    /// Writes to the legacy APIC MMIO page.
    pub fn mmio_write(&mut self, address: u64, data: &[u8]) {
        if !self.apic.xapic_enabled() || !is_valid_apic_access(address) {
            tracelimit::warn_ratelimited!(
                address,
                len = data.len(),
                enabled = self.apic.hardware_enabled(),
                x2apic = self.apic.x2apic_enabled(),
                "invalid apic write"
            );
            return;
        }

        let mut value = [0; 4];
        let offset = address as usize & 3;
        let data = &data[..data.len().min(4 - offset)];
        value[offset..offset + data.len()].copy_from_slice(data);

        self.write_register(
            ApicRegister((address >> 4) as u8),
            u32::from_ne_bytes(value),
        );
    }

    /// Reads from the APIC base MSR, X2APIC MSR, or Hyper-V enlightenment MSR.
    pub fn msr_read(&mut self, msr: u32) -> Result<u64, MsrError> {
        let v = match msr {
            X86X_MSR_APIC_BASE => self.apic.apic_base,
            X2APIC_MSR_BASE..=X2APIC_MSR_END if self.apic.x2apic_enabled() => {
                let register = ApicRegister((msr - X2APIC_MSR_BASE) as u8);
                if register == ApicRegister::ICR0 {
                    // ICR is a 64-bit register in X2APIC.
                    self.apic.icr
                } else {
                    self.read_register(register)
                        .ok_or(MsrError::InvalidAccess)?
                        .into()
                }
            }
            hvdef::HV_X64_MSR_APIC_FREQUENCY if self.apic.global.hyperv_enlightenments => {
                TIMER_FREQUENCY
            }
            hvdef::HV_X64_MSR_EOI if self.apic.global.hyperv_enlightenments => {
                return Err(MsrError::InvalidAccess)
            }
            hvdef::HV_X64_MSR_ICR if self.apic.global.hyperv_enlightenments => {
                if !self.apic.hardware_enabled() {
                    return Err(MsrError::InvalidAccess);
                }
                self.apic.icr
            }
            hvdef::HV_X64_MSR_TPR if self.apic.global.hyperv_enlightenments => {
                (self.client.cr8() << 4) as u64
            }
            _ => return Err(MsrError::Unknown),
        };
        Ok(v)
    }

    /// Writes to the APIC base MSR or an X2APIC MSR.
    pub fn msr_write(&mut self, msr: u32, value: u64) -> Result<(), MsrError> {
        match msr {
            X86X_MSR_APIC_BASE => {
                // The APIC may be disabled by this, so we need IRR/ISR local to
                // be reset.
                self.ensure_state_local();
                match self.apic.set_apic_base_inner(value) {
                    Ok(()) => self.client.set_apic_base(self.apic.apic_base),
                    Err(err) => tracelimit::warn_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        "invalid apic base write"
                    ),
                }
            }
            X2APIC_MSR_BASE..=X2APIC_MSR_END if self.apic.x2apic_enabled() => {
                let register = ApicRegister((msr - X2APIC_MSR_BASE) as u8);
                if register == ApicRegister::ICR0 {
                    // ICR is a 64-bit register in X2APIC.
                    self.apic.icr = value & u64::from(ICR_X2APIC_MASK);
                    self.handle_ipi(Icr::from(self.apic.icr));
                } else if !self.write_register(register, value as u32) {
                    return Err(MsrError::InvalidAccess);
                }
            }
            hvdef::HV_X64_MSR_APIC_FREQUENCY if self.apic.global.hyperv_enlightenments => {
                return Err(MsrError::InvalidAccess)
            }
            hvdef::HV_X64_MSR_EOI if self.apic.global.hyperv_enlightenments => {
                if !self.apic.hardware_enabled() {
                    return Err(MsrError::InvalidAccess);
                }
                self.eoi(false);
            }
            hvdef::HV_X64_MSR_ICR if self.apic.global.hyperv_enlightenments => {
                let mask = if self.apic.x2apic_enabled() {
                    ICR_X2APIC_MASK
                } else if self.apic.xapic_enabled() {
                    ICR_XAPIC_MASK
                } else {
                    return Err(MsrError::InvalidAccess);
                };
                self.apic.icr = value & u64::from(mask);
                self.handle_ipi(Icr::from(self.apic.icr));
            }
            hvdef::HV_X64_MSR_TPR if self.apic.global.hyperv_enlightenments => {
                if value > 0xff {
                    return Err(MsrError::InvalidAccess);
                }
                self.client.set_cr8((value as u32) >> 4);
            }
            _ => return Err(MsrError::Unknown),
        }
        Ok(())
    }

    fn read_register(&mut self, register: ApicRegister) -> Option<u32> {
        let value = match register {
            ApicRegister::ID => self.apic.id_register(),
            ApicRegister::VERSION => self.apic.version,
            ApicRegister::TPR => self.client.cr8() << 4,
            ApicRegister::PPR => {
                self.ensure_state_local();
                let task_pri = self.client.cr8();
                let isr_pri = priority(self.apic.isr.top().unwrap_or(0));
                task_pri.max(isr_pri.into()) << 4
            }
            ApicRegister::LDR => self.apic.ldr_register(),
            ApicRegister::DFR if !self.apic.x2apic_enabled() => {
                if self.apic.cluster_mode {
                    Dfr::CLUSTERED_MODE.0
                } else {
                    Dfr::FLAT_MODE.0
                }
            }
            ApicRegister::SVR => self.apic.svr,
            reg if (ApicRegister::ISR0..=ApicRegister::ISR7).contains(&reg) => {
                self.ensure_state_local();
                let index = reg.0 - ApicRegister::ISR0.0;
                self.apic.isr.to_bits()[index as usize]
            }
            reg if (ApicRegister::TMR0..=ApicRegister::TMR7).contains(&reg) => {
                self.apic.pull_irr();
                let index = reg.0 - ApicRegister::TMR0.0;
                self.apic.tmr[index as usize]
            }
            reg if (ApicRegister::IRR0..=ApicRegister::IRR7).contains(&reg) => {
                self.ensure_state_local();
                self.apic.pull_irr();
                let index = reg.0 - ApicRegister::IRR0.0;
                self.apic.irr[index as usize]
            }
            ApicRegister::ESR => self.apic.esr,
            ApicRegister::ICR0 if !self.apic.x2apic_enabled() => self.apic.icr as u32,
            ApicRegister::ICR1 if !self.apic.x2apic_enabled() => (self.apic.icr >> 32) as u32,
            ApicRegister::LVT_TIMER => self.apic.lvt_timer,
            ApicRegister::LVT_THERMAL => self.apic.lvt_thermal,
            ApicRegister::LVT_PMC => self.apic.lvt_pmc,
            ApicRegister::LVT_LINT0 => self.apic.lvt_lint[0],
            ApicRegister::LVT_LINT1 => self.apic.lvt_lint[1],
            ApicRegister::LVT_ERROR => self.apic.lvt_error,
            ApicRegister::TIMER_ICR => self.apic.timer_icr,
            ApicRegister::TIMER_CCR => {
                self.apic.eval_time(self.client.now());
                self.apic.timer_ccr
            }
            ApicRegister::TIMER_DCR => self.apic.timer_dcr,
            register => {
                tracelimit::warn_ratelimited!(?register, "unimplemented apic register read");
                return None;
            }
        };
        Some(value)
    }

    fn write_register(&mut self, register: ApicRegister, value: u32) -> bool {
        match register {
            ApicRegister::TPR => {
                self.client.set_cr8(value >> 4);
            }
            ApicRegister::EOI => {
                if self.apic.x2apic_enabled() && value != 0 {
                    return false;
                }
                self.eoi(false);
            }
            ApicRegister::LDR if !self.apic.x2apic_enabled() => {
                self.apic.ldr = value & 0xff000000;
                self.apic.update_slot();
            }
            ApicRegister::DFR if !self.apic.x2apic_enabled() => {
                self.apic.cluster_mode = cluster_mode(value);
                self.apic.update_slot();
            }
            ApicRegister::SVR => {
                // The APIC may be disabled by this, so we need to reevaluate
                // offloading.
                self.ensure_state_local();
                // Accumulate any requested interrupts before changing the
                // enable state.
                self.apic.pull_irr();
                self.apic.svr = value & u32::from(Svr::new().with_vector(0xff).with_enable(true));
                if !self.apic.software_enabled() {
                    // Mask all the LVTs.
                    for lvt in [
                        &mut self.apic.lvt_timer,
                        &mut self.apic.lvt_thermal,
                        &mut self.apic.lvt_pmc,
                        &mut self.apic.lvt_error,
                    ]
                    .into_iter()
                    .chain(&mut self.apic.lvt_lint)
                    {
                        *lvt = Lvt::from(*lvt).with_masked(true).into();
                    }
                }
                self.apic.update_slot();
            }
            ApicRegister::ESR => {
                if self.apic.x2apic_enabled() && value != 0 {
                    return false;
                }
                // This would copy and zero the hidden error register, but we
                // never set that to a non-zero value and don't include it in
                // the saved-state format.
                self.apic.esr = 0;
            }
            ApicRegister::ICR0 if !self.apic.x2apic_enabled() => {
                self.apic.icr = (value as u64 | (self.apic.icr & 0xffffffff_00000000))
                    & u64::from(ICR_XAPIC_MASK);

                self.handle_ipi(self.apic.icr.into());
            }
            ApicRegister::ICR1 if !self.apic.x2apic_enabled() => {
                self.apic.icr = (((value as u64) << 32) | self.apic.icr & 0xffffffff)
                    & u64::from(ICR_XAPIC_MASK);
            }
            ApicRegister::LVT_TIMER => {
                self.apic.lvt_timer = self.apic.effective_lvt(
                    value
                        & u32::from(
                            Lvt::new()
                                .with_vector(0xff)
                                .with_masked(true)
                                .with_timer_mode(1), // no TSC deadline support
                        ),
                );
            }
            ApicRegister::LVT_THERMAL => {
                self.apic.lvt_thermal = self.apic.effective_lvt(
                    value
                        & u32::from(
                            Lvt::new()
                                .with_vector(0xff)
                                .with_delivery_mode(0b111)
                                .with_masked(true),
                        ),
                );
            }
            ApicRegister::LVT_PMC => {
                self.apic.lvt_pmc = self.apic.effective_lvt(
                    value
                        & u32::from(
                            Lvt::new()
                                .with_vector(0xff)
                                .with_delivery_mode(0b111)
                                .with_masked(true),
                        ),
                );
            }
            reg @ (ApicRegister::LVT_LINT0 | ApicRegister::LVT_LINT1) => {
                let index = if reg == ApicRegister::LVT_LINT0 { 0 } else { 1 };
                self.apic.lvt_lint[index] = self.apic.effective_lvt(
                    value
                        & u32::from(
                            Lvt::new()
                                .with_vector(0xff)
                                .with_input_pin_polarity(true)
                                .with_trigger_mode_level(true)
                                .with_delivery_mode(0b111)
                                .with_masked(true),
                        ),
                );
                self.apic.update_slot();
            }
            ApicRegister::LVT_ERROR => {
                self.apic.lvt_error = self.apic.effective_lvt(
                    value & u32::from(Lvt::new().with_vector(0xff).with_masked(true)),
                );
            }
            ApicRegister::TIMER_ICR => {
                let now = self.client.now();
                self.apic.timer_icr = value;
                self.apic.timer_ccr = value;
                self.apic.last_time = now;
                self.apic.update_timeout(now);
            }
            ApicRegister::TIMER_DCR => {
                let now = self.client.now();
                self.apic.eval_time(now);
                self.apic.timer_dcr =
                    value & u32::from(Dcr::new().with_value_low(0b11).with_value_high(0b1));
                self.apic.update_timeout(now);
            }
            ApicRegister::SELF_IPI if self.apic.x2apic_enabled() => {
                self.apic.stats.self_ipi.increment();
                self.apic.shared.request_interrupt(
                    self.apic.software_enabled(),
                    DeliveryMode::FIXED,
                    value as u8,
                    false,
                    false,
                );
            }
            register => {
                tracelimit::warn_ratelimited!(?register, "unimplemented apic register write");
                return false;
            }
        }
        true
    }

    fn ensure_state_local(&mut self) {
        if self.apic.is_offloaded {
            let (irr, isr) = self.client.pull_offload();
            self.apic.accumulate_from_offload(&irr, &isr);
            self.apic.stats.offload_pull.increment();

            // Make sure that we commit any bits we read from the offloaded apic
            // before we dispatch back to the vp, since pull offload clears
            // corresponding bits in the offloaded apic state. Otherwise, we
            // could commit state bits which would result in the guest being in
            // a broken state.
            self.apic.needs_offload_reeval = true;
        }
    }

    fn handle_ipi(&mut self, icr: Icr) {
        tracing::trace!(?icr, vp = self.apic.shared.vp_index.index(), "ipi");

        let delivery_mode = DeliveryMode(icr.delivery_mode());
        match delivery_mode {
            DeliveryMode::FIXED => {}
            DeliveryMode::LOWEST_PRIORITY => {
                if self.apic.x2apic_enabled() {
                    // Don't allow lowest priority IPIs via x2apic.
                    return;
                }
            }
            DeliveryMode::NMI => {}
            DeliveryMode::INIT => {
                // Ignore INIT level deasserts here.
                if !icr.level_assert() {
                    return;
                }
            }
            DeliveryMode::SIPI => {}
            DeliveryMode::EXTINT => {
                // Not allowed as an IPI.
                return;
            }
            _ => return,
        }

        match DestinationShorthand(icr.destination_shorthand()) {
            DestinationShorthand::NONE => {
                let destination = Destination::from_icr(icr, self.apic.x2apic_enabled());
                match destination {
                    Destination::Physical(_) | Destination::Logical(_) => {
                        self.apic.stats.other_ipi.increment()
                    }
                    Destination::Broadcast | Destination::AllExcept(_) => {
                        self.apic.stats.broadcast_ipi.increment()
                    }
                }
                self.apic.global.request_interrupt(
                    destination,
                    delivery_mode,
                    icr.vector(),
                    false,
                    |vp| self.client.wake(vp),
                );
            }
            DestinationShorthand::SELF => {
                self.apic.stats.self_ipi.increment();
                self.apic.shared.request_interrupt(
                    self.apic.software_enabled(),
                    delivery_mode,
                    icr.vector(),
                    icr.trigger_mode_level(),
                    false,
                );
            }
            DestinationShorthand::ALL_INCLUDING_SELF => {
                self.apic.stats.broadcast_ipi.increment();
                self.apic.global.request_interrupt(
                    Destination::Broadcast,
                    delivery_mode,
                    icr.vector(),
                    false,
                    |vp| self.client.wake(vp),
                );
            }
            DestinationShorthand::ALL_EXCLUDING_SELF => {
                self.apic.stats.broadcast_ipi.increment();
                self.apic.global.request_interrupt(
                    Destination::AllExcept(self.apic.id),
                    delivery_mode,
                    icr.vector(),
                    false,
                    |vp| self.client.wake(vp),
                );
            }
            _ => unreachable!(),
        }
    }
}

impl SharedState {
    /// Returns true if the VP should be woken up to scan the APIC.
    fn request_interrupt(
        &self,
        software_enabled: bool,
        delivery_mode: DeliveryMode,
        vector: u8,
        level_triggered: bool,
        auto_eoi: bool,
    ) -> bool {
        tracing::trace!(
            software_enabled,
            ?delivery_mode,
            vector,
            level_triggered,
            vp = self.vp_index.index(),
            "interrupt"
        );

        match delivery_mode {
            DeliveryMode::FIXED | DeliveryMode::LOWEST_PRIORITY => {
                if !software_enabled || !(16..=255).contains(&vector) {
                    return false;
                }
                let (bank, mask) = bank_mask(vector);
                if (self.tmr[bank].load(Ordering::Relaxed) & mask != 0) != level_triggered {
                    if level_triggered {
                        self.tmr[bank].fetch_or(mask, Ordering::Relaxed);
                    } else {
                        self.tmr[bank].fetch_and(!mask, Ordering::Relaxed);
                    }
                }
                if (self.auto_eoi[bank].load(Ordering::Relaxed) & mask != 0) != auto_eoi {
                    if auto_eoi {
                        self.auto_eoi[bank].fetch_or(mask, Ordering::Relaxed);
                    } else {
                        self.auto_eoi[bank].fetch_and(!mask, Ordering::Relaxed);
                    }
                }
                if self.new_irr[bank].fetch_or(mask, Ordering::Release) & mask == 0 {
                    return true;
                }
                false
            }
            DeliveryMode::NMI => {
                let old = self
                    .work
                    .fetch_update(Ordering::Release, Ordering::Relaxed, |w| {
                        Some(WorkFlags::from(w).with_nmi(true).into())
                    })
                    .unwrap();
                old == 0
            }
            DeliveryMode::INIT => {
                let old = self
                    .work
                    .fetch_update(Ordering::Release, Ordering::Relaxed, |w| {
                        Some(WorkFlags::from(w).with_init(true).into())
                    })
                    .unwrap();
                old == 0
            }
            DeliveryMode::SIPI => {
                let old = self
                    .work
                    .fetch_update(Ordering::Release, Ordering::Relaxed, |w| {
                        Some(
                            WorkFlags::from(w)
                                .with_sipi(true)
                                .with_sipi_vector(vector)
                                .into(),
                        )
                    })
                    .unwrap();
                old == 0
            }
            DeliveryMode::EXTINT => {
                let old = self
                    .work
                    .fetch_update(Ordering::Release, Ordering::Relaxed, |w| {
                        Some(WorkFlags::from(w).with_extint(true).into())
                    })
                    .unwrap();
                old == 0
            }
            _ => false,
        }
    }
}

impl MutableGlobalState {
    fn request_broadcast_interrupt(
        &self,
        delivery_mode: DeliveryMode,
        vector: u8,
        level_triggered: bool,
        wake: impl FnMut(VpIndex),
    ) {
        self.request_set_interrupt(delivery_mode, vector, level_triggered, wake, |_, _| true);
    }

    fn request_set_interrupt(
        &self,
        delivery_mode: DeliveryMode,
        vector: u8,
        level_triggered: bool,
        mut wake: impl FnMut(VpIndex),
        mut filter: impl FnMut(u32, &ApicSlot) -> bool,
    ) {
        let lowest_priority = delivery_mode == DeliveryMode::LOWEST_PRIORITY;
        for (apic_id, slot) in self.by_apic_id.iter().enumerate() {
            if !filter(apic_id as u32, slot) {
                continue;
            }
            // For now, just pick the first enabled APIC in the set for lowest priority.
            if !lowest_priority || slot.software_enabled {
                slot.request_interrupt(delivery_mode, vector, level_triggered, false, &mut wake);
                if lowest_priority {
                    break;
                }
            }
        }
    }
}

impl ApicSlot {
    fn request_interrupt(
        &self,
        delivery_mode: DeliveryMode,
        vector: u8,
        level_triggered: bool,
        auto_eoi: bool,
        wake: impl FnOnce(VpIndex),
    ) {
        if let Some(shared) = &self.shared {
            if self.hardware_enabled
                && shared.request_interrupt(
                    self.software_enabled,
                    delivery_mode,
                    vector,
                    level_triggered,
                    auto_eoi,
                )
            {
                wake(shared.vp_index);
            }
        }
    }
}

/// Work to do as a result of [`LocalApic::scan`] or [`LocalApic::flush`].
#[derive(Debug, Default)]
pub struct ApicWork {
    /// An INIT interrupt was requested.
    ///
    /// Reset register state (including APIC state) as documented in the Intel
    /// manual.
    pub init: bool,
    /// A SIPI interrupt was requested with the given vector.
    ///
    /// Update the cs and rip to the appropriate values and clear the
    /// wait-for-SIPI state.
    pub sipi: Option<u8>,
    /// An extint interrupt was requested.
    ///
    /// When the processor is ready for extint injection, query the PIC for the
    /// vector and inject the interrupt.
    pub extint: bool,
    /// An NMI was requested.
    pub nmi: bool,
    /// A fixed interrupt was requested.
    ///
    /// Call [`LocalApic::acknowledge_interrupt`] after it has been injected.
    pub interrupt: Option<u8>,
}

/// An error writing the APIC base MSR.
#[derive(Debug, Error)]
pub enum InvalidApicBase {
    /// Invalid x2apic state.
    #[error("invalid x2apic state")]
    InvalidX2Apic,
    /// Can't disable x2apic without reset.
    #[error("can't disable x2apic without reset")]
    CantDisableX2Apic,
}

/// APIC offload is not supported with the current request state, likely due to
/// auto EOI. The caller must disable offloads and scan the APIC again.
pub struct OffloadNotSupported;

impl LocalApic {
    /// Returns an object to access APIC registers.
    pub fn access<'a, T: ApicClient>(&'a mut self, client: &'a mut T) -> LocalApicAccess<'_, T> {
        LocalApicAccess { apic: self, client }
    }

    /// Fast path for updating IRR on the local processor.
    pub fn request_fixed_interrupts(&mut self, mut irr: [u32; 8]) {
        if self.hardware_enabled() && self.software_enabled() {
            // Don't allow setting invalid bits.
            irr[0] &= !0xffff;
            for (bank, &irr) in irr.iter().enumerate() {
                self.irr[bank] |= irr;
                self.tmr[bank] &= !irr;
                self.auto_eoi[bank] &= !irr;
            }
            self.needs_offload_reeval = true;
            self.recompute_next_irr();
        }
    }

    /// Gets the APIC base MSR.
    pub fn apic_base(&self) -> u64 {
        self.apic_base
    }

    /// Gets the APIC base address, if the APIC is enabled and in xapic mode.
    pub fn base_address(&self) -> Option<u64> {
        self.base_address
    }

    /// Sets the APIC base MSR.
    ///
    /// Returns false if the value is invalid.
    pub fn set_apic_base(&mut self, apic_base: u64) -> Result<(), InvalidApicBase> {
        assert!(
            !self.is_offloaded,
            "failed to onload before setting the APIC base"
        );
        self.set_apic_base_inner(apic_base)
    }

    /// The caller must ensure that the offloaded APIC state is local.
    fn set_apic_base_inner(&mut self, apic_base: u64) -> Result<(), InvalidApicBase> {
        let current = ApicBase::from(self.apic_base);

        // Only allow changing the enable and x2apic enable bits.
        let new = ApicBase::from(apic_base);
        let new = current.with_enable(new.enable()).with_x2apic(new.x2apic());

        tracing::debug!(
            ?current,
            ?new,
            apic_base,
            vp = self.shared.vp_index.index(),
            "update apic base"
        );

        if new.x2apic() && (!new.enable() || !self.global.x2apic_capable) {
            // Invalid x2apic state.
            return Err(InvalidApicBase::InvalidX2Apic);
        }

        if current.x2apic() && new.enable() && !new.x2apic() {
            // Can't disable x2apic once it is enabled without going through a
            // reset or disable.
            return Err(InvalidApicBase::CantDisableX2Apic);
        }

        if current.enable() && !new.enable() {
            self.reset_registers();
        }

        self.apic_base = new.into();
        self.update_slot();
        Ok(())
    }

    fn hardware_enabled(&self) -> bool {
        ApicBase::from(self.apic_base).enable()
    }

    fn xapic_enabled(&self) -> bool {
        self.hardware_enabled() && !self.x2apic_enabled()
    }

    fn x2apic_enabled(&self) -> bool {
        ApicBase::from(self.apic_base).x2apic()
    }

    fn software_enabled(&self) -> bool {
        Svr::from(self.svr).enable()
    }

    /// Sets the masked bit in an LVT if the APIC is software disabled.
    fn effective_lvt(&self, lvt: u32) -> u32 {
        let mut lvt = Lvt::from(lvt);
        if !self.software_enabled() {
            lvt.set_masked(true);
        }
        lvt.into()
    }

    /// Scans for pending interrupts.
    pub fn scan(&mut self, vmtime: &mut VmTimeAccess, scan_irr: bool) -> ApicWork {
        if !self.hardware_enabled() {
            return Default::default();
        }

        if let Some(next) = self.next_timeout {
            let now = vmtime.now();
            if now.is_after(next) {
                self.eval_time(now);
                self.update_timeout(now);
            }
            if let Some(next) = self.next_timeout {
                vmtime.set_timeout_if_before(next);
            }
        }

        let mut r = self.flush();
        if scan_irr {
            self.pull_irr();
        }
        if !self.is_offloaded {
            r.interrupt = self.next_irr();
        }

        r
    }

    fn next_irr(&self) -> Option<u8> {
        if !self.software_enabled() {
            return None;
        }
        let vector = self.next_irr?;
        let pri = priority(vector);
        if self.isr.top().map_or(0, priority) < pri {
            Some(vector)
        } else {
            None
        }
    }

    /// Handles APIC offload, calling `update` with new bits in IRR, ISR, and
    /// the current value of TMR.
    ///
    /// `update` should accumulate IRR and ISR into the offload APIC page and
    /// update the EOI exit bitmap if TMR has changed since the last call.
    ///
    /// `update` will not be called if there are no changes (i.e. if IRR and ISR
    /// are both zero).
    pub fn push_to_offload(
        &mut self,
        update: impl FnOnce(&[u32; 8], &[u32; 8], &[u32; 8]),
    ) -> Result<(), OffloadNotSupported> {
        if self.needs_offload_reeval && self.is_offloaded && self.software_enabled() {
            if self.active_auto_eoi {
                return Err(OffloadNotSupported);
            }
            update(&self.irr, &self.isr.to_bits(), &self.tmr);
            self.irr = [0; 8];
            self.isr.clear();
            self.stats.offload_push.increment();
            self.needs_offload_reeval = false;
        }
        Ok(())
    }

    /// Returns whether APIC offload is enabled.
    pub fn is_offloaded(&self) -> bool {
        self.is_offloaded
    }

    /// Returns true if it is safe to set an IRR bit directly in offloaded APIC
    /// state.
    pub fn can_offload_irr(&self) -> bool {
        self.is_offloaded && self.software_enabled()
    }

    /// Enables APIC offload.
    pub fn enable_offload(&mut self) {
        self.is_offloaded = true;
        self.needs_offload_reeval = true;
    }

    /// Disables APIC offload, accumulating IRR and ISR from the offload APIC
    /// page.
    pub fn disable_offload(&mut self, irr: &[u32; 8], isr: &[u32; 8]) {
        self.accumulate_from_offload(irr, isr);
        self.is_offloaded = false;
    }

    fn accumulate_from_offload(&mut self, irr: &[u32; 8], isr: &[u32; 8]) {
        let mut local_isr = self.isr.to_bits();

        // TODO: We probably should instead not touch ISR at all unless we are
        // about to disable offload. Refactor this later.
        assert!(self.is_offloaded);

        for (((local_irr, &remote_irr), local_isr), &remote_isr) in
            self.irr.iter_mut().zip(irr).zip(&mut local_isr).zip(isr)
        {
            *local_irr |= remote_irr;
            *local_isr |= remote_isr;
        }
        self.isr.load_from_bits(local_isr);
        self.recompute_next_irr();
        self.needs_offload_reeval = true;
    }

    /// Flushes work as in [`Self::scan`], but does not poll timers or IRR.
    ///
    /// This must be called before [`Self::save`] to flush hidden state to
    /// registers.
    pub fn flush(&mut self) -> ApicWork {
        if self.shared.work.load(Ordering::Relaxed) == 0 {
            return Default::default();
        }

        let mut r = ApicWork::default();
        let work = WorkFlags::from(self.shared.work.swap(0, Ordering::SeqCst));
        if work.init() {
            self.stats.init.increment();
            r.init = true;
        }
        if work.sipi() {
            self.stats.sipi.increment();
            r.sipi = Some(work.sipi_vector());
        }
        if work.nmi() {
            self.stats.nmi.increment();
            r.nmi = true;
        }
        if work.extint() {
            self.stats.extint.increment();
            r.extint = true;
        }

        r
    }

    /// Acknowledges the interrupt returned by `scan`.
    pub fn acknowledge_interrupt(&mut self, vector: u8) {
        assert!(!self.is_offloaded);
        assert_eq!(Some(vector), self.next_irr);
        let (bank, mask) = bank_mask(vector);
        self.irr[bank] &= !mask;
        self.recompute_next_irr();
        if self.auto_eoi[bank] & mask == 0 {
            self.isr.push(vector);
        }
        self.stats.interrupt.increment();
    }

    /// Returns whether an EOI is pending that can be completed lazily, without
    /// intercepting the VP.
    pub fn is_lazy_eoi_pending(&self) -> bool {
        if self.is_offloaded {
            return false;
        }
        let eoi_vector = if let Some(next_irr) = self.next_irr {
            // There is at least one pending interrupt. Allow lazy EOI only if
            // there are no in-service interrupts:
            //
            // 1. If there are any in-service interrupts with a higher priority,
            //    then we need an EOI intercept to know when to inject the
            //    pending interrupt.
            //
            // 2. If there are any in-service interrupts with a lower priority,
            //    then the pending interrupt is pending injection, so it would
            //    be ambiguous whether the lazy EOI was for the in-service
            //    interrupt or the pending interrupt.
            if !self.isr.is_empty() {
                return false;
            }

            // Only allow lazy EOI if next_irr is the only irr
            let (bank, mask) = bank_mask(next_irr);
            let mut expected = [0; 8];
            expected[bank] = mask;
            if !expected.iter().eq(self.irr.iter()) {
                return false;
            }

            next_irr
        } else if let Some(vector) = self.isr.top() {
            // There are no pending interrupts. Allow lazy EOI for the top
            // in-service interrupt.
            vector
        } else {
            return false;
        };

        // Only allow lazy EOI if the interrupt is edge-triggered. Otherwise, we
        // need an intercept to check whether to reassert the interrupt.
        let (bank, mask) = bank_mask(eoi_vector);
        self.tmr[bank] & mask == 0
    }

    fn eval_time(&mut self, now: VmTime) {
        if self.timer_ccr == 0 {
            return;
        }

        let shift = dcr_divider_shift(Dcr::from(self.timer_dcr));

        let raw_nanos = now.checked_sub(self.last_time).unwrap().as_nanos() as u64;
        let counts = (raw_nanos / NANOS_PER_TICK) >> shift;

        let lvt = Lvt::from(self.lvt_timer);
        if counts >= self.timer_ccr as u64 {
            if !lvt.masked() {
                self.shared.request_interrupt(
                    self.software_enabled(),
                    DeliveryMode::FIXED,
                    lvt.vector(),
                    false,
                    false,
                );
            }

            if TimerMode(lvt.timer_mode()) == TimerMode::ONE_SHOT {
                self.timer_ccr = 0;
                // Don't bother to update the last eval time.
                return;
            } else {
                let remaining = counts - self.timer_ccr as u64;
                // Avoid the divide in the common case.
                if remaining < self.timer_icr as u64 {
                    self.timer_ccr = self.timer_icr - remaining as u32;
                } else {
                    self.timer_ccr = self.timer_icr - (remaining % self.timer_icr as u64) as u32;
                }
            }
        } else {
            self.timer_ccr -= counts as u32;
        }

        let elapsed_nanos = (counts << shift) * NANOS_PER_TICK;

        self.last_time = self
            .last_time
            .wrapping_add(Duration::from_nanos(elapsed_nanos));
    }

    fn update_timeout(&mut self, now: VmTime) {
        self.next_timeout = (self.timer_ccr != 0).then(|| {
            let counts = self.timer_ccr;
            let ticks = (counts as u64) << dcr_divider_shift(Dcr::from(self.timer_dcr));
            now.wrapping_add(Duration::from_nanos(ticks * NANOS_PER_TICK))
        });
    }

    /// Resets the APIC state.
    pub fn reset(&mut self) {
        assert!(!self.is_offloaded);

        self.apic_base = ApicBase::new()
            .with_base_page(APIC_BASE_PAGE)
            .with_bsp(self.shared.vp_index.is_bsp())
            .with_enable(true)
            .into();

        self.reset_registers();
        // Drop any pending requests.
        self.shared.work.store(0, Ordering::Relaxed);
    }

    fn reset_registers(&mut self) {
        let Self {
            shared: _,
            global: _,
            apic_base: _,
            base_address: _,
            id: _,
            version: _,
            ldr,
            cluster_mode,
            svr,
            isr,
            next_irr,
            irr,
            tmr,
            auto_eoi,
            esr,
            icr,
            lvt_timer,
            lvt_thermal,
            lvt_pmc,
            lvt_lint,
            lvt_error,
            timer_icr,
            timer_ccr,
            last_time: _,
            next_timeout,
            timer_dcr,
            active_auto_eoi,
            needs_offload_reeval,
            is_offloaded: _,
            stats: _,
        } = self;

        *ldr = 0;
        *cluster_mode = false;
        *svr = 0xff;
        isr.clear();
        *esr = 0;
        *icr = 0;
        *next_irr = None;
        // Note that any bits in `shared.new_irr` will be cleared and ignored by
        // the next call to `pull_irr` since the APIC is now in a software
        // disabled state.
        *irr = [0; 8];
        *needs_offload_reeval = false;
        *tmr = [0; 8];
        *auto_eoi = [0; 8];
        *active_auto_eoi = false;
        for lvt in [lvt_timer, lvt_thermal, lvt_pmc, lvt_error]
            .into_iter()
            .chain(lvt_lint)
        {
            *lvt = Lvt::new().with_masked(true).into();
        }
        *timer_icr = 0;
        *timer_ccr = 0;
        *timer_dcr = 0;
        *next_timeout = None;
        self.update_slot();
    }

    fn update_slot(&mut self) {
        // Cache the base address, since `base_address()` is called in the
        // instruction emulator hot path.
        self.base_address = self
            .xapic_enabled()
            .then(|| (ApicBase::from(self.apic_base).base_page() as u64) << 12);

        let mut mutable = self.global.mutable.write();
        let mutable = &mut *mutable;
        let slot = &mut mutable.by_apic_id[self.id as usize];
        slot.lint = self.lvt_lint.map(Lvt::from);
        slot.logical_id = (self.ldr >> 24) as u8;
        slot.hardware_enabled = self.hardware_enabled();
        slot.software_enabled = self.software_enabled();

        mutable.x2apic_enabled -= slot.x2apic_enabled as usize;
        let apic_base = ApicBase::from(self.apic_base);
        slot.x2apic_enabled = apic_base.enable() && apic_base.x2apic();
        mutable.x2apic_enabled += slot.x2apic_enabled as usize;

        mutable.logical_cluster_mode -= slot.cluster_mode as usize;
        slot.cluster_mode = self.cluster_mode;
        mutable.logical_cluster_mode += slot.cluster_mode as usize;
    }

    /// Returns the APIC register state.
    pub fn save(&mut self) -> virt::x86::vp::Apic {
        assert!(!self.is_offloaded, "failed to disable offload before save");

        // Ensure any pending interrupt requests have been pulled into the local
        // state.
        self.pull_irr();

        let registers = ApicRegisters {
            reserved_0: [0; 2],
            id: self.id_register(),
            version: self.version,
            reserved_4: [0; 4],
            tpr: 0, // TODO
            apr: 0,
            ppr: 0,
            eoi: 0,
            rrd: 0,
            ldr: self.ldr_register(),
            dfr: if self.x2apic_enabled() {
                0
            } else if self.cluster_mode {
                Dfr::CLUSTERED_MODE.0
            } else {
                Dfr::FLAT_MODE.0
            },
            svr: self.svr,
            isr: self.isr.to_bits(),
            tmr: self.tmr,
            irr: self.irr,
            esr: self.esr,
            reserved_29: [0; 6],
            lvt_cmci: 0,
            icr: [self.icr as u32, (self.icr >> 32) as u32],
            lvt_timer: self.lvt_timer,
            lvt_thermal: self.lvt_thermal,
            lvt_pmc: self.lvt_pmc,
            lvt_lint0: self.lvt_lint[0],
            lvt_lint1: self.lvt_lint[1],
            lvt_error: self.lvt_error,
            timer_icr: self.timer_icr,
            timer_ccr: 0,
            reserved_3a: [0; 4],
            timer_dcr: self.timer_dcr,
            reserved_3f: 0,
        };
        virt::x86::vp::Apic {
            apic_base: self.apic_base,
            registers: registers.into(),
            auto_eoi: self.auto_eoi,
        }
    }

    /// Restores the APIC register state.
    pub fn restore(&mut self, state: &virt::x86::vp::Apic) -> Result<(), InvalidApicBase> {
        assert!(!self.is_offloaded);

        let virt::x86::vp::Apic {
            apic_base,
            registers,
            auto_eoi,
        } = state;

        self.set_apic_base_inner(*apic_base)?;

        // No register modifications allowed if the APIC is disabled.
        if !self.hardware_enabled() {
            return Ok(());
        }

        let ApicRegisters {
            reserved_0: _,
            id,
            version,
            reserved_4: _,
            tpr: _,
            apr: _,
            ppr: _,
            eoi: _,
            rrd: _,
            ldr,
            dfr,
            svr,
            isr,
            tmr,
            irr,
            esr,
            reserved_29: _,
            lvt_cmci: _,
            icr,
            lvt_timer,
            lvt_thermal,
            lvt_pmc,
            lvt_lint0,
            lvt_lint1,
            lvt_error,
            timer_icr,
            timer_ccr: _,
            reserved_3a: _,
            timer_dcr,
            reserved_3f: _,
        } = registers.into();

        self.id = if self.x2apic_enabled() { id } else { id >> 24 };
        self.version = version;
        if !self.x2apic_enabled() {
            self.ldr = ldr & 0xff000000;
        }
        self.cluster_mode = cluster_mode(dfr);
        self.svr = svr;
        self.irr = irr;
        self.tmr = tmr;
        self.auto_eoi = *auto_eoi;
        self.recompute_next_irr();
        self.isr.clear();
        self.isr.load_from_bits(isr);
        self.esr = esr;
        self.icr = icr[0] as u64 | ((icr[1] as u64) << 32);
        self.lvt_timer = self.effective_lvt(lvt_timer);
        self.lvt_thermal = self.effective_lvt(lvt_thermal);
        self.lvt_pmc = self.effective_lvt(lvt_pmc);
        self.lvt_lint = [self.effective_lvt(lvt_lint0), self.effective_lvt(lvt_lint1)];
        self.lvt_error = self.effective_lvt(lvt_error);
        self.timer_icr = timer_icr;
        self.timer_dcr = timer_dcr;
        self.update_slot();
        self.needs_offload_reeval = true;
        Ok(())
    }

    fn recompute_next_irr(&mut self) {
        for (i, &v) in self.irr.iter().enumerate().rev() {
            if v != 0 {
                let vector = (i as u32) * 32 + (31 - v.leading_zeros());
                self.next_irr = Some(vector as u8);
                return;
            }
        }
        self.next_irr = None;
        self.active_auto_eoi = false;
    }

    /// Read all the remote IRR bits into the local IRR array. Having two arrays
    /// like this ensures that we don't miss an interrupt if a second instance
    /// of one arrives while the first instance is being injected into the
    /// processor.
    ///
    /// Hypervisor backends that acknowledge interrupts before running the VP
    /// would never hit this condition, because the VP would not have a chance
    /// to run code to act upon the interrupt, and the two interrupts could be
    /// merged.
    ///
    /// But hypervisor backends that acknowledge interrupts only after running
    /// the VP for some time (such as those backed by AMD SNP) could hit this,
    /// since the VP will act on the interrupt, which might cause a device or
    /// another processor to generate a second interrupt before the first VP
    /// exits and acknowledges the first interrupt.
    fn pull_irr(&mut self) {
        for (
            ((((local_irr, local_tmr), local_auto_eoi), remote_irr), remote_tmr),
            remote_auto_eoi,
        ) in self
            .irr
            .iter_mut()
            .zip(&mut self.tmr)
            .zip(&mut self.auto_eoi)
            .zip(&self.shared.new_irr)
            .zip(&self.shared.tmr)
            .zip(&self.shared.auto_eoi)
        {
            // Read `irr` first with acquire ordering so that the TMR bit
            // associated with each requested interrupt is correct.
            if remote_irr.load(Ordering::Relaxed) == 0 {
                continue;
            }
            let irr = remote_irr.swap(0, Ordering::Acquire);
            let tmr = remote_tmr.load(Ordering::Relaxed);
            let auto_eoi = remote_auto_eoi.load(Ordering::Relaxed);
            if Svr::from(self.svr).enable() {
                *local_irr |= irr;
                *local_tmr &= !irr;
                *local_tmr |= tmr & irr;
                *local_auto_eoi &= !irr;
                *local_auto_eoi |= auto_eoi & irr;
                self.active_auto_eoi |= auto_eoi != 0;
                self.needs_offload_reeval = true;
            }
        }
        self.recompute_next_irr();
    }

    fn id_register(&self) -> u32 {
        if self.x2apic_enabled() {
            self.id
        } else {
            self.id << 24
        }
    }

    fn ldr_register(&self) -> u32 {
        if self.x2apic_enabled() {
            X2ApicLogicalId::new()
                .with_cluster_id((self.id >> 4) as u16)
                .with_logical_id(1 << (self.id & 0xf))
                .into()
        } else {
            self.ldr
        }
    }
}
