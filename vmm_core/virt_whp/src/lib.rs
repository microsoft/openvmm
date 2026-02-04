// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! WHP implementation of the virt::generic interfaces.

#![cfg(all(windows, guest_is_native))]
#![expect(missing_docs)]
// UNSAFETY: Calling WHP APIs and manually managing memory.
#![expect(unsafe_code)]
#![expect(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]

mod apic;
pub mod device;
mod emu;
mod hypercalls;
mod memory;
mod regs;
mod synic;
mod vm_state;
mod vp;
mod vp_state;
mod vtl2;

use crate::memory::vtl2_mapper::MappingState;
use crate::memory::vtl2_mapper::ResetMappingState;
use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use hv1_emulator::hv::GlobalHv;
use hv1_emulator::hv::GlobalHvParams;
use hv1_emulator::hv::ProcessorVtlHv;
use hv1_emulator::message_queues::MessageQueues;
use hv1_structs::VtlSet;
use hvdef::HvDeliverabilityNotificationsRegister;
use hvdef::HvMessage;
use hvdef::HvMessageType;
use hvdef::Vtl;
use inspect::Inspect;
use inspect::InspectMut;
use memory::MemoryMapper;
use memory_range::MemoryRange;
use parking_lot::Mutex;
use parking_lot::RwLock;
use range_map_vec::RangeMap;
use std::convert::Infallible;
use std::ops::Index;
use std::ops::IndexMut;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Waker;
use thiserror::Error;
use virt::IsolationType;
use virt::NeedsYield;
use virt::PageVisibility;
use virt::PartitionAccessState;
use virt::PartitionConfig;
use virt::ProtoPartition;
use virt::ProtoPartitionConfig;
use virt::StopVp;
use virt::VpHaltReason;
use virt::VpIndex;
use virt::io::CpuIo;
use virt::irqcon::MsiRequest;
use virt::vm::AccessVmState;
use vm_topology::memory::MemoryLayout;
use vm_topology::processor::TargetVpInfo;
use vmcore::monitor::MonitorPage;
use vmcore::reference_time::GetReferenceTime;
use vmcore::reference_time::ReferenceTimeResult;
use vmcore::reference_time::ReferenceTimeSource;
use vmcore::vmtime::VmTimeAccess;
use vmcore::vmtime::VmTimeSource;
use vp_state::WhpVpStateAccess;
use x86defs::cpuid::Vendor;

#[cfg(guest_arch = "aarch64")]
pub use aarch64::WHP_PMU_GSIV;

#[derive(Debug)]
pub struct Whp;

#[derive(Inspect)]
#[inspect(transparent)]
pub struct WhpPartition {
    inner: Arc<WhpPartitionInner>,
    #[inspect(skip)]
    with_vtl0: Arc<WhpPartitionAndVtl>,
    #[inspect(skip)]
    with_vtl2: Option<Arc<WhpPartitionAndVtl>>,
}

#[derive(Inspect)]
pub struct WhpPartitionInner {
    vtl0: VtlPartition,
    vtl2: Option<VtlPartition>,
    #[inspect(skip)]
    vps: Vec<WhpVp>,
    #[inspect(skip)]
    mem_layout: MemoryLayout,
    #[inspect(skip)]
    gm: GuestMemory,
    vtl2_emulation: Option<vtl2::Vtl2Emulation>,
    #[cfg(guest_arch = "x86_64")]
    irq_routes: virt::irqcon::IrqRoutes,
    #[inspect(skip)]
    caps: virt::PartitionCapabilities,
    #[cfg(guest_arch = "x86_64")]
    cpuid: virt::CpuidLeafSet,
    vtl0_alias_map_offset: Option<u64>,
    monitor_page: MonitorPage,
    hvstate: Hv1State,
    isolation: IsolationType,
}

#[derive(Inspect)]
struct VtlPartition {
    #[inspect(skip)]
    whp: whp::Partition,
    #[inspect(skip)]
    vplcs: Vec<Vplc>,
    #[inspect(with = "|x| inspect::adhoc(|req| inspect::iter_by_index(&*x.read()).inspect(req))")]
    ranges: RwLock<Vec<memory::MappedRange>>,

    /// Virtual PCI device interrupt remapping table. This is used instead of
    /// the hypervisor's implementation so that system-level privileges (needed
    /// to get a VPCIVSP handle) are not required for software VPCI devices.
    #[cfg(guest_arch = "x86_64")]
    software_devices: virt::x86::apic_software_device::ApicSoftwareDevices,

    /// Memory mapper implementation that supports either VTLs or overlays.
    mapper: Box<dyn MemoryMapper>,

    lapic: LocalApicKind,

    hypervisor_enlightened: bool,
}

#[derive(Inspect)]
#[inspect(external_tag)]
enum LocalApicKind {
    #[cfg(guest_arch = "x86_64")]
    #[inspect(transparent)]
    Emulated(virt_support_apic::LocalApicSet),
    Offloaded,
}

#[derive(Inspect)]
#[inspect(external_tag)]
enum Hv1State {
    Disabled,
    #[inspect(transparent)]
    Emulated(GlobalHv<3>),
    Offloaded,
}

#[derive(Debug)]
struct WhpVp {
    interrupt: NeedsYield,
    vtl2_wake: AtomicBool,
    /// Enable VTL2 at the next opportunity.
    vtl2_enable: AtomicBool,
    /// Force reset of run state at the next run.
    reset_next: AtomicBool,
    /// Scrub VTL2 state at the next run.
    scrub_next: AtomicBool,
    vp_info: TargetVpInfo,
    waker: RwLock<Option<Waker>>,
    #[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
    scan_irr: AtomicBool,
}

#[derive(InspectMut)]
struct RunState {
    #[inspect(with = "|x| *x as u8")]
    active_vtl: Vtl,
    enabled_vtls: VtlSet,
    runnable_vtls: VtlSet,
    #[inspect(hex, with = "|&x| u64::from(x)")]
    vtl2_deliverability_notifications: HvDeliverabilityNotificationsRegister,
    vtl2_wakeup_vmtime: Option<VmTimeAccess>,
    #[inspect(skip)]
    finish_reset_vtl0: bool,
    #[inspect(skip)]
    finish_reset_vtl2: bool,
    crash_msg_address: Option<u64>,
    crash_msg_len: Option<usize>,
    #[inspect(flatten)]
    vtls: RunStateVtls,
    #[inspect(mut)]
    halted: bool,
    exits: vp::ExitStats,
    vmtime: VmTimeAccess,
}

#[derive(Inspect)]
struct RunStateVtls {
    vtl0: PerVtlRunState,
    vtl2: Option<PerVtlRunState>,
}

#[derive(Inspect)]
struct PerVtlRunState {
    #[cfg(guest_arch = "x86_64")]
    lapic: Option<apic::ApicState>,
    hv: Option<ProcessorVtlHv>,
    #[inspect(hex, with = "|&x| u64::from(x)")]
    deliverability_notifications: HvDeliverabilityNotificationsRegister,
    // Only used when `hv` is `None`.
    vp_assist_page: u64,
}

impl PerVtlRunState {
    pub fn new(lapic: &LocalApicKind, hv: &Hv1State, vp_info: &TargetVpInfo, vtl: Vtl) -> Self {
        #[cfg(guest_arch = "aarch64")]
        let LocalApicKind::Offloaded = lapic;
        Self {
            #[cfg(guest_arch = "x86_64")]
            lapic: if let LocalApicKind::Emulated(lapic) = lapic {
                Some(apic::ApicState::new(lapic, vp_info))
            } else {
                None
            },
            hv: if let Hv1State::Emulated(hv) = hv {
                Some(hv.add_vp(vp_info.base.vp_index, vtl))
            } else {
                None
            },
            deliverability_notifications: 0.into(),
            vp_assist_page: 0,
        }
    }

    fn vp_assist_page(&self) -> Option<u64> {
        let val = if let Some(hv) = &self.hv {
            hv.vp_assist_page()
        } else {
            self.vp_assist_page
        };
        if val & 1 != 0 {
            Some(val & !0xfff)
        } else {
            None
        }
    }
}

impl RunStateVtls {
    #[cfg(guest_arch = "x86_64")]
    fn lapic(&mut self, vtl: Vtl) -> Option<&mut apic::ApicState> {
        self[vtl].lapic.as_mut()
    }
}

impl Index<Vtl> for RunStateVtls {
    type Output = PerVtlRunState;

    fn index(&self, vtl: Vtl) -> &Self::Output {
        match vtl {
            Vtl::Vtl0 => &self.vtl0,
            Vtl::Vtl1 => unreachable!(),
            Vtl::Vtl2 => self.vtl2.as_ref().unwrap(),
        }
    }
}

impl IndexMut<Vtl> for RunStateVtls {
    fn index_mut(&mut self, vtl: Vtl) -> &mut Self::Output {
        match vtl {
            Vtl::Vtl0 => &mut self.vtl0,
            Vtl::Vtl1 => unreachable!(),
            Vtl::Vtl2 => self.vtl2.as_mut().unwrap(),
        }
    }
}

impl RunState {
    fn reset(&mut self, vtl2_scrub: bool, is_bsp: bool) {
        let &mut Self {
            ref mut active_vtl,
            ref mut runnable_vtls,
            ref mut vtl2_deliverability_notifications,
            enabled_vtls,
            ref mut crash_msg_address,
            ref mut crash_msg_len,
            ref mut vtls,
            ref mut halted,
            finish_reset_vtl0: ref mut reset_vtl0,
            finish_reset_vtl2: ref mut reset_vtl2,
            exits: _,
            vtl2_wakeup_vmtime: _,
            vmtime: _,
        } = self;

        *runnable_vtls = enabled_vtls;
        *active_vtl = runnable_vtls.highest_set().unwrap();
        *vtl2_deliverability_notifications = Default::default();
        *crash_msg_address = None;
        *crash_msg_len = None;
        if !vtl2_scrub {
            vtls.vtl0.reset(is_bsp);
            *reset_vtl0 = true;
        }
        if let Some(vtl) = &mut vtls.vtl2 {
            vtl.reset(is_bsp);
            *reset_vtl2 = true;
        }
        *halted = false;
    }
}

impl PerVtlRunState {
    fn reset(&mut self, is_bsp: bool) {
        let Self {
            #[cfg(guest_arch = "x86_64")]
            lapic,
            hv,
            deliverability_notifications,
            vp_assist_page,
        } = self;

        #[cfg(guest_arch = "x86_64")]
        if let Some(lapic) = lapic {
            lapic.reset(is_bsp);
        }

        if let Some(hv) = hv {
            hv.reset();
        }

        #[cfg(guest_arch = "aarch64")]
        let _ = is_bsp;

        *deliverability_notifications = 0.into();
        *vp_assist_page = 0;
    }
}

impl WhpVp {
    fn new(vp: TargetVpInfo, active_vtl: Vtl) -> Self {
        let vtl2_enabled = active_vtl >= Vtl::Vtl2;
        Self {
            interrupt: NeedsYield::new(),
            vtl2_wake: false.into(),
            vtl2_enable: vtl2_enabled.into(),
            reset_next: false.into(),
            scrub_next: false.into(),
            vp_info: vp,
            waker: Default::default(),
            scan_irr: true.into(),
        }
    }
}

#[cfg(guest_arch = "x86_64")]
type InitialVpContext = hvdef::hypercall::InitialVpContextX64;
#[cfg(guest_arch = "aarch64")]
type InitialVpContext = hvdef::hypercall::InitialVpContextArm64;

#[derive(Debug, Inspect)]
struct Vplc {
    message_queues: MessageQueues,
    check_queues: AtomicBool,
    extint_pending: AtomicBool,
    #[inspect(with = "|x| x.lock().is_some()")]
    start_vp_context: Mutex<Option<Box<InitialVpContext>>>,
    #[inspect(skip)]
    start_vp: AtomicBool,
}

impl Vplc {
    fn new() -> Self {
        Self {
            message_queues: MessageQueues::new(),
            check_queues: false.into(),
            extint_pending: false.into(),
            start_vp: false.into(),
            start_vp_context: Default::default(),
        }
    }
}

impl<'a> WhpVpRef<'a> {
    fn vp(&self) -> &'a WhpVp {
        &self.partition.vps[self.index.index() as usize]
    }

    fn vplc(&self, vtl: Vtl) -> &'a Vplc {
        match vtl {
            Vtl::Vtl0 => &self.partition.vtl0.vplcs[self.index.index() as usize],
            Vtl::Vtl1 => unreachable!(),
            Vtl::Vtl2 => &self.partition.vtl2.as_ref().unwrap().vplcs[self.index.index() as usize],
        }
    }

    fn whp(&self, vtl: Vtl) -> whp::Processor<'a> {
        match vtl {
            Vtl::Vtl0 => self.partition.vtl0.whp.vp(self.index.index()),
            Vtl::Vtl1 => unreachable!(),
            Vtl::Vtl2 => self
                .partition
                .vtl2
                .as_ref()
                .unwrap()
                .whp
                .vp(self.index.index()),
        }
    }

    fn ensure_vtl_runnable(&self, vtl: Vtl) {
        if vtl > Vtl::Vtl0 {
            if !self.vp().vtl2_wake.swap(true, Ordering::SeqCst) {
                self.whp(Vtl::Vtl0).cancel_run().expect("can't fail");
            }
        }
    }

    fn wake(&self) {
        if let Some(waker) = &*self.vp().waker.read() {
            waker.wake_by_ref();
        }
    }

    // Enqueues a message to be posted when the associated message slot is free.
    fn post_message(&self, vtl: Vtl, sint: u8, message: &HvMessage) {
        let request_notification = self.vplc(vtl).message_queues.enqueue_message(sint, message);

        if request_notification {
            self.vplc(vtl).check_queues.store(true, Ordering::SeqCst);
            self.ensure_vtl_runnable(vtl);
            self.wake();
        }
    }
}

impl virt::ResetPartition for WhpPartition {
    type Error = Error;

    fn reset(&self) -> Result<(), Error> {
        self.inner.vtl0.reset()?;
        for vp in self.inner.vps() {
            vp.vp().reset_next.store(true, Ordering::SeqCst);
        }
        self.validate_is_reset(Vtl::Vtl0);

        if let Some(vtl2) = self.inner.vtl2.as_ref() {
            vtl2.reset()?;
            self.validate_is_reset(Vtl::Vtl2);
            self.inner
                .vtl2_emulation
                .as_ref()
                .expect("should be set")
                .reset(true);
        }

        self.inner.hvstate.reset(&self.inner.gm);

        Ok(())
    }
}

impl virt::ScrubVtl for WhpPartition {
    type Error = Error;

    fn scrub(&self, vtl: Vtl) -> Result<(), Error> {
        assert!(!self.inner.isolation.is_isolated());
        assert_eq!(vtl, Vtl::Vtl2);

        tracing::info!(?vtl, "scrubbing partition");

        let vtl2 = self.inner.vtl2.as_ref().ok_or(Error::NoVtl2)?;

        // Preserve VTL2 reference time across the scrub to match hypervisor
        // behavior and so that the guest can determine how much time was lost.
        // This isn't exactly correct, because the time to reset the partition
        // is not included, but it's good enough for VTL2 simulation.
        #[cfg(guest_arch = "x86_64")]
        let reference_time = self.access_state(Vtl::Vtl2).reftime()?;

        // NOTE: Mapping state (and therefore VTL protections) is _not_ reset
        // across scrub. Thus only reset WHP state, but not VtlPartition state.
        vtl2.whp.reset().for_op("reset partition")?;
        for vp in self.inner.vps() {
            vp.vp().scrub_next.store(true, Ordering::SeqCst);
        }
        self.inner.vtl2_emulation.as_ref().unwrap().reset(false);
        self.validate_is_reset(Vtl::Vtl2);

        #[cfg(guest_arch = "x86_64")]
        {
            self.access_state(Vtl::Vtl2).set_reftime(&reference_time)?;
        }
        Ok(())
    }
}

impl virt::AcceptInitialPages for WhpPartition {
    type Error = Error;

    fn accept_initial_pages(&self, pages: &[(MemoryRange, PageVisibility)]) -> Result<(), Error> {
        assert!(self.inner.isolation.is_isolated());

        for (range, vis) in pages {
            self.inner
                .vtl0
                .accept_pages(range, *vis)
                .map_err(Error::AcceptPages)?;

            if let Some(vtl2) = &self.inner.vtl2 {
                vtl2.accept_pages(range, *vis).map_err(Error::AcceptPages)?;
            }
        }

        Ok(())
    }
}

impl virt::Partition for WhpPartition {
    fn supports_reset(&self) -> Option<&dyn virt::ResetPartition<Error = Error>> {
        if whp::capabilities::reset_partition() {
            Some(self)
        } else {
            None
        }
    }

    fn supports_vtl_scrub(
        &self,
    ) -> Option<&dyn virt::ScrubVtl<Error = <Self as virt::Hv1>::Error>> {
        (!self.inner.isolation.is_isolated()).then_some(self)
    }

    fn supports_initial_accept_pages(
        &self,
    ) -> Option<&dyn virt::AcceptInitialPages<Error = <Self as virt::Hv1>::Error>> {
        self.inner.isolation.is_isolated().then_some(self)
    }

    fn doorbell_registration(
        self: &Arc<Self>,
        minimum_vtl: Vtl,
    ) -> Option<Arc<dyn DoorbellRegistration>> {
        Some(self.with_vtl(minimum_vtl).clone())
    }

    #[cfg(guest_arch = "x86_64")]
    fn msi_interrupt_target(
        self: &Arc<Self>,
        minimum_vtl: Vtl,
    ) -> Option<Arc<dyn pci_core::msi::MsiInterruptTarget>> {
        Some(self.with_vtl(minimum_vtl).clone())
    }

    fn request_msi(&self, vtl: Vtl, request: MsiRequest) {
        if let Err(err) = self.inner.interrupt(vtl, request) {
            tracelimit::warn_ratelimited!(
                address = request.address,
                data = request.data,
                error = &err as &dyn std::error::Error,
                "failed to request msi"
            );
        }
    }

    fn caps(&self) -> &virt::PartitionCapabilities {
        &self.inner.caps
    }

    fn request_yield(&self, vp_index: VpIndex) {
        if self
            .inner
            .vp(vp_index)
            .unwrap()
            .vp()
            .interrupt
            .request_yield()
        {
            // For now, cancel both VTLs to ensure we stop.
            self.inner
                .vtl0
                .whp
                .vp(vp_index.index())
                .cancel_run()
                .expect("cancel should never fail");
            if let Some(vtl2) = &self.inner.vtl2 {
                vtl2.whp
                    .vp(vp_index.index())
                    .cancel_run()
                    .expect("cancel should never fail");
            }
        }
    }
}

#[cfg(guest_arch = "x86_64")]
impl virt::X86Partition for WhpPartition {
    fn ioapic_routing(&self) -> Arc<dyn virt::irqcon::IoApicRouting> {
        self.inner.clone()
    }

    fn pulse_lint(&self, vp_index: VpIndex, vtl: Vtl, lint: u8) {
        self.inner.lint(vp_index, vtl, lint.into());
    }
}

#[cfg(guest_arch = "aarch64")]
impl virt::Aarch64Partition for WhpPartition {
    fn control_gic(&self, vtl: Vtl) -> Arc<dyn virt::irqcon::ControlGic> {
        self.with_vtl(vtl).clone()
    }
}

pub struct WhpProcessorBinder {
    partition: Arc<WhpPartitionInner>,
    index: VpIndex,
    run_state: Option<RunState>,
}

impl virt::BindProcessor for WhpProcessorBinder {
    type Processor<'a> = WhpProcessor<'a>;
    type Error = Error;

    fn bind(&mut self) -> Result<Self::Processor<'_>, Self::Error> {
        let vp = WhpProcessor {
            vp: WhpVpRef {
                partition: &self.partition,
                index: self.index,
            },
            inner: &self.partition.vps[self.index.index() as usize],
            state: self.run_state.take().unwrap(),
            vplc0: &self.partition.vtl0.vplcs[self.index.index() as usize],
            vplc2: self
                .partition
                .vtl2
                .as_ref()
                .map(|vtl2| &vtl2.vplcs[self.index.index() as usize]),
        };

        let vp_info = &vp.inner.vp_info;

        for (vtl, vtlp) in self.partition.vtlps() {
            // Set the initial APIC ID.
            #[cfg(guest_arch = "x86_64")]
            if vp_info.apic_id != vp_info.base.vp_index.index() {
                if let LocalApicKind::Offloaded = vtlp.lapic {
                    vp.vp
                        .whp(vtl)
                        .set_register(whp::Register64::InitialApicId, vp_info.apic_id.into())
                        .for_op("set initial apic id")?;
                }
            }

            #[cfg(all(guest_arch = "aarch64", not(feature = "unstable_whp")))]
            {
                let _ = vp_info;
                let _ = vtlp;
                let _ = vtl;
            }

            #[cfg(all(guest_arch = "aarch64", feature = "unstable_whp"))]
            {
                let _ = vtlp;
                vp.vp
                    .whp(vtl)
                    .set_register(whp::Register64::MpidrEl1, vp_info.mpidr.into())
                    .for_op("set mpidr")?;
                vp.vp
                    .whp(vtl)
                    .set_register(whp::Register64::GicrBaseGpa, vp_info.gicr)
                    .for_op("set GICR base")?;
            }
        }

        Ok(vp)
    }
}

#[derive(InspectMut)]
pub struct WhpProcessor<'a> {
    #[inspect(skip)]
    vp: WhpVpRef<'a>,
    #[inspect(skip)]
    inner: &'a WhpVp,
    #[inspect(flatten, mut)]
    state: RunState,
    vplc0: &'a Vplc,
    vplc2: Option<&'a Vplc>,
}

#[derive(Copy, Clone)]
struct WhpVpRef<'a> {
    partition: &'a WhpPartitionInner,
    index: VpIndex,
}

// TODO: Chunk this up into smaller types.
#[derive(Error, Debug)]
pub enum Error {
    #[error("whp error, failed to {operation}")]
    Whp {
        operation: &'static str,
        #[source]
        source: whp::WHvError,
    },
    #[error("guest debugging not supported")]
    GuestDebuggingNotSupported,
    #[error(transparent)]
    State(#[from] Box<virt::state::StateError<Error>>),
    #[error("this operation requires vtl2 emulation")]
    NoVtl2,
    #[error("failed to create virtual device")]
    NewDevice(#[source] virt::x86::apic_software_device::DeviceIdInUse),
    #[error("resetting memory mappings failed")]
    ResetMemoryMapping(#[source] anyhow::Error),
    #[error("accepting pages failed")]
    AcceptPages(#[source] anyhow::Error),
    #[error("invalid apic base")]
    InvalidApicBase(#[source] virt_support_apic::InvalidApicBase),
    #[error("host does not support required cpu capabilities")]
    Capabilities(virt::PartitionCapabilitiesError),
}

trait WhpResultExt<T> {
    fn for_op(self, op: &'static str) -> Result<T, Error>;
}

impl<T> WhpResultExt<T> for Result<T, whp::WHvError> {
    fn for_op(self, op: &'static str) -> Result<T, Error> {
        self.map_err(|err| Error::Whp {
            operation: op,
            source: err,
        })
    }
}

impl virt::Hypervisor for Whp {
    type ProtoPartition<'a> = WhpProtoPartition<'a>;
    type Partition = WhpPartition;
    type Error = Error;

    fn new_partition<'a>(
        &mut self,
        config: ProtoPartitionConfig<'a>,
    ) -> Result<WhpProtoPartition<'a>, Error> {
        let vtl0 = VtlPartition::new(&config, Vtl::Vtl0)?;
        let vtl2 = if config
            .hv_config
            .as_ref()
            .is_some_and(|cfg| cfg.vtl2.is_some())
        {
            Some(VtlPartition::new(&config, Vtl::Vtl2)?)
        } else {
            None
        };

        Ok(WhpProtoPartition { vtl0, vtl2, config })
    }

    fn is_available(&self) -> Result<bool, Error> {
        whp::capabilities::hypervisor_present().for_op("query hypervisor presence")
    }
}

/// The prototype partition.
pub struct WhpProtoPartition<'a> {
    vtl0: VtlPartition,
    vtl2: Option<VtlPartition>,
    config: ProtoPartitionConfig<'a>,
}

impl ProtoPartition for WhpProtoPartition<'_> {
    type Partition = WhpPartition;
    type ProcessorBinder = WhpProcessorBinder;
    type Error = Error;

    #[cfg(guest_arch = "x86_64")]
    fn cpuid(&self, eax: u32, ecx: u32) -> [u32; 4] {
        // This call should never fail unless there is a kernel or hypervisor
        // bug.
        let output = self
            .vtl0
            .whp
            .vp(0)
            .get_cpuid_output(eax, ecx)
            .expect("cpuid should not fail");

        [output.Eax, output.Ebx, output.Ecx, output.Edx]
    }

    #[cfg(guest_arch = "x86_64")]
    fn max_physical_address_size(&self) -> u8 {
        virt::x86::max_physical_address_size_from_cpuid(&|eax, ecx| self.cpuid(eax, ecx))
    }

    #[cfg(not(guest_arch = "x86_64"))]
    fn max_physical_address_size(&self) -> u8 {
        self.vtl0
            .whp
            .physical_address_width()
            .unwrap()
            .try_into()
            .unwrap()
    }

    fn build(
        self,
        config: PartitionConfig<'_>,
    ) -> Result<(Self::Partition, Vec<Self::ProcessorBinder>), Self::Error> {
        let inner = Arc::new(WhpPartitionInner::new(
            config,
            &self.config,
            self.vtl0,
            self.vtl2,
        )?);

        let with_vtl0 = Arc::new(WhpPartitionAndVtl {
            partition: inner.clone(),
            vtl: Vtl::Vtl0,
        });
        let with_vtl2 = inner.vtl2.as_ref().map(|_| {
            Arc::new(WhpPartitionAndVtl {
                partition: inner.clone(),
                vtl: Vtl::Vtl2,
            })
        });

        let partition = WhpPartition {
            inner,
            with_vtl0,
            with_vtl2,
        };
        partition.validate_is_reset(Vtl::Vtl0);
        if partition.inner.vtl2.is_some() {
            partition.validate_is_reset(Vtl::Vtl2);
        }
        let vps = partition
            .inner
            .vps()
            .map(|vp| {
                let mut enabled_vtls = VtlSet::new();
                enabled_vtls.set(Vtl::Vtl0);
                if vp.vp().vtl2_enable.load(Ordering::Relaxed) {
                    enabled_vtls.set(Vtl::Vtl2);
                }
                let vtl2_wakeup_vmtime = partition.inner.vtl2.is_some().then(|| {
                    self.config
                        .vmtime
                        .access(format!("vtl2-wakeup-{}", vp.index.index()))
                });
                WhpProcessorBinder {
                    partition: partition.inner.clone(),
                    index: vp.index,
                    run_state: Some(RunState {
                        active_vtl: enabled_vtls.highest_set().unwrap(),
                        enabled_vtls,
                        runnable_vtls: enabled_vtls,
                        vtl2_deliverability_notifications: Default::default(),
                        finish_reset_vtl0: true,
                        finish_reset_vtl2: partition.inner.vtl2.is_some(),
                        crash_msg_address: None,
                        crash_msg_len: None,
                        halted: false,
                        vtls: RunStateVtls {
                            vtl0: PerVtlRunState::new(
                                &partition.inner.vtl0.lapic,
                                &partition.inner.hvstate,
                                &vp.vp().vp_info,
                                Vtl::Vtl0,
                            ),
                            vtl2: partition.inner.vtl2.as_ref().map(|p| {
                                PerVtlRunState::new(
                                    &p.lapic,
                                    &partition.inner.hvstate,
                                    &vp.vp().vp_info,
                                    Vtl::Vtl2,
                                )
                            }),
                        },
                        vtl2_wakeup_vmtime,
                        vmtime: self
                            .config
                            .vmtime
                            .access(format!("vp-{}", vp.index.index())),
                        exits: Default::default(),
                    }),
                }
            })
            .collect::<Vec<_>>();

        Ok((partition, vps))
    }
}

impl WhpPartitionInner {
    fn new(
        config: PartitionConfig<'_>,
        proto_config: &ProtoPartitionConfig<'_>,
        vtl0: VtlPartition,
        vtl2: Option<VtlPartition>,
    ) -> Result<Self, Error> {
        // FUTURE: register cpuid results with the hypervisor, and register
        // appropriate per-VP results where necessary (or tell the hypervisor
        // the AMD topology information so that it can provide per-VP results
        // accurately).
        #[cfg(guest_arch = "x86_64")]
        let cpuid = {
            use vm_topology::processor::x86::ApicMode;

            let mut cpuid = Vec::new();
            // Report x2apic support. When the APIC is in the hypervisor, the
            // hypervisor will do this automatically, but it doesn't hurt to do this
            // again.
            let mask = [0, 0, 1 << 21, 0];
            let (value, use_apic_msrs) = match proto_config.processor_topology.apic_mode() {
                ApicMode::XApic => ([0; 4], true),
                ApicMode::X2ApicSupported | ApicMode::X2ApicEnabled => (mask, false),
            };
            cpuid.push(
                virt::CpuidLeaf::new(x86defs::cpuid::CpuidFunction::VersionAndFeatures.0, value)
                    .masked(mask),
            );

            // Add in the synthetic hv leaves if necessary.
            if let Some(hv_config) = &proto_config.hv_config {
                if !hv_config.offload_enlightenments || proto_config.user_mode_apic {
                    let enlightenments = hvdef::HvEnlightenmentInformation::new()
                        .with_deprecate_auto_eoi(true)
                        .with_use_relaxed_timing(true)
                        .with_use_ex_processor_masks(true)
                        .with_use_apic_msrs(use_apic_msrs);
                    const MAX_CPUS: u32 = 2048;
                    cpuid.extend_from_slice(
                        &hv1_emulator::cpuid::make_hv_cpuid_leaves(
                            hv1_emulator::cpuid::SUPPORTED_FEATURES,
                            enlightenments,
                            MAX_CPUS,
                        )
                        .map(|(f, v)| virt::CpuidLeaf::new(f.0, [v.eax, v.ebx, v.ecx, v.edx])),
                    );
                    hv1_emulator::cpuid::process_hv_cpuid_leaves(&mut cpuid, false, [0; 4]);
                }
            }

            cpuid.extend(config.cpuid);
            virt::CpuidLeafSet::new(cpuid)
        };

        let mut vtl0_alias_map_offset = None;
        let vtl2_emulation = if let Some(vtl2_config) = proto_config
            .hv_config
            .as_ref()
            .and_then(|cfg| cfg.vtl2.as_ref())
        {
            vtl0_alias_map_offset = config.vtl0_alias_map;

            // TODO: Supporting the alias map with isolation requires additional
            // mapper changes that are not implemented yet.
            if vtl0_alias_map_offset.is_some() && proto_config.isolation.is_isolated() {
                todo!("alias map and isolation requires memory mapper changes")
            }

            if let Some(late_map_config) = vtl2_config.late_map_vtl0_memory.as_ref() {
                let mapper = vtl2.as_ref().expect("must be set").mapper.as_ref();
                match &late_map_config.allowed_ranges {
                    virt::LateMapVtl0AllowedRanges::MemoryLayout => {
                        let vtl2_range = config
                            .mem_layout
                            .vtl2_range()
                            .expect("no vtl2 range when expected");
                        mapper.add_allowed_range(vtl2_range);
                    }
                    virt::LateMapVtl0AllowedRanges::Ranges(ranges) => {
                        for range in ranges {
                            mapper.add_allowed_range(*range);
                        }
                    }
                }
            }

            Some(vtl2::Vtl2Emulation::new(
                vtl2_config
                    .late_map_vtl0_memory
                    .as_ref()
                    .map(|cfg| cfg.policy)
                    .unwrap_or(virt::LateMapVtl0MemoryPolicy::Log),
            ))
        } else {
            None
        };

        let vps = proto_config
            .processor_topology
            .vps_arch()
            .map(|vp| {
                WhpVp::new(
                    vp,
                    if vp.base.vp_index.is_bsp() && vtl2.is_some() {
                        Vtl::Vtl2
                    } else {
                        Vtl::Vtl0
                    },
                )
            })
            .collect();

        #[cfg(guest_arch = "x86_64")]
        let caps = {
            let mut caps = virt::x86::X86PartitionCapabilities::from_cpuid(
                proto_config.processor_topology,
                &mut |function, index| {
                    let output = vtl0
                        .whp
                        .vp(0)
                        .get_cpuid_output(function, index)
                        .expect("cpuid should not fail");
                    cpuid.result(
                        function,
                        index,
                        &[output.Eax, output.Ebx, output.Ecx, output.Edx],
                    )
                },
            )
            .map_err(Error::Capabilities)?;
            caps.can_freeze_time = true;
            caps.xsaves_state_bv_broken = true;
            caps.dr6_tsx_broken = true;
            caps
        };
        #[cfg(guest_arch = "aarch64")]
        let caps = virt::aarch64::Aarch64PartitionCapabilities {};

        let vendor = match whp::capabilities::processor_vendor().for_op("get processor vendor")? {
            whp::abi::WHvProcessorVendorIntel => Vendor::INTEL,
            #[cfg(guest_arch = "x86_64")]
            whp::abi::WHvProcessorVendorAmd => Vendor::AMD,
            #[cfg(guest_arch = "x86_64")]
            whp::abi::WHvProcessorVendorHygon => Vendor::HYGON,
            #[cfg(guest_arch = "aarch64")]
            whp::abi::WHvProcessorVendorArm => Vendor([0; 12]),
            _ => panic!("unsupported processor vendor"),
        };

        let hvstate = if proto_config.hv_config.is_some() {
            if vtl0.hypervisor_enlightened {
                Hv1State::Offloaded
            } else {
                let tsc_frequency = vtl0.whp.tsc_frequency().for_op("get tsc frequency")?;
                let ref_time = ReferenceTimeSource::new(VmTimeReferenceTimeSource::new(
                    proto_config.vmtime.clone(),
                ));
                Hv1State::Emulated(GlobalHv::new(GlobalHvParams {
                    max_vp_count: proto_config.processor_topology.vp_count(),
                    vendor,
                    tsc_frequency,
                    ref_time,
                    is_ref_time_backed_by_tsc: false,
                }))
            }
        } else {
            Hv1State::Disabled
        };

        let inner = Self {
            vtl0,
            vtl2,
            vps,
            mem_layout: config.mem_layout.clone(),
            gm: config.guest_memory.clone(),
            vtl2_emulation,
            #[cfg(guest_arch = "x86_64")]
            irq_routes: Default::default(),
            caps,
            #[cfg(guest_arch = "x86_64")]
            cpuid,
            vtl0_alias_map_offset,
            monitor_page: MonitorPage::new(),
            hvstate,
            isolation: proto_config.isolation,
        };

        Ok(inner)
    }

    fn vtlp(&self, vtl: Vtl) -> &VtlPartition {
        match vtl {
            Vtl::Vtl0 => &self.vtl0,
            Vtl::Vtl1 => unreachable!(),
            Vtl::Vtl2 => self.vtl2.as_ref().unwrap(),
        }
    }

    fn vtlps(&self) -> impl DoubleEndedIterator<Item = (Vtl, &VtlPartition)> {
        std::iter::once((Vtl::Vtl0, &self.vtl0)).chain(self.vtl2.as_ref().map(|p| (Vtl::Vtl2, p)))
    }

    fn bsp(&self) -> WhpVpRef<'_> {
        WhpVpRef {
            partition: self,
            index: VpIndex::BSP,
        }
    }

    fn vp(&self, index: VpIndex) -> Option<WhpVpRef<'_>> {
        if index.index() >= self.vps.len() as u32 {
            return None;
        }
        Some(WhpVpRef {
            partition: self,
            index,
        })
    }

    #[cfg(guest_arch = "x86_64")]
    fn vp_by_apic_id(&self, apic_id: u32) -> Option<WhpVpRef<'_>> {
        self.vps().find(|vp| vp.vp().vp_info.apic_id == apic_id)
    }

    fn vps(&self) -> impl Iterator<Item = WhpVpRef<'_>> {
        (0..self.vps.len() as u32).map(|i| WhpVpRef {
            partition: self,
            index: VpIndex::new(i),
        })
    }

    fn post_message(&self, vtl: Vtl, vp: VpIndex, sint: u8, typ: HvMessageType, payload: &[u8]) {
        let message = HvMessage::new(typ, 0, payload);
        let Some(vpref) = self.vp(vp) else {
            tracelimit::warn_ratelimited!(vp = vp.index(), "invalid vp for post message");
            return;
        };
        match &self.hvstate {
            Hv1State::Offloaded | Hv1State::Emulated(_) => {
                vpref.post_message(vtl, sint, &message);
            }
            Hv1State::Disabled => {
                tracelimit::warn_ratelimited!(
                    ?vtl,
                    vp = vp.index(),
                    sint,
                    ?typ,
                    "no synic configured, dropping message"
                );
            }
        }
    }
}

/// A time implementation based on VmTime.
impl GetReferenceTime for VmTimeReferenceTimeSource {
    fn now(&self) -> ReferenceTimeResult {
        ReferenceTimeResult {
            ref_time: self.vmtime.now().as_100ns(),
            system_time: None,
        }
    }
}

struct VmTimeReferenceTimeSource {
    vmtime: VmTimeAccess,
}

impl VmTimeReferenceTimeSource {
    fn new(vmtime: VmTimeSource) -> Self {
        VmTimeReferenceTimeSource {
            vmtime: vmtime.access("reftime"),
        }
    }
}

impl VtlPartition {
    fn new(config: &ProtoPartitionConfig<'_>, vtl: Vtl) -> Result<Self, Error> {
        let mut hypervisor_enlightened = false;

        let mut extended_exits = whp::abi::WHV_EXTENDED_VM_EXITS(0);

        let user_mode_apic = config.user_mode_apic
            || config
                .hv_config
                .as_ref()
                .is_some_and(|cfg| !cfg.offload_enlightenments);

        #[cfg(guest_arch = "x86_64")]
        let lapic = if user_mode_apic {
            let x2apic_capable = !matches!(
                config.processor_topology.apic_mode(),
                vm_topology::processor::x86::ApicMode::XApic
            );
            if x2apic_capable {
                // Needed to set the x2apic bit.
                extended_exits |= whp::abi::WHV_EXTENDED_VM_EXITS::X64CpuidExit;
            }
            let lapic = virt_support_apic::LocalApicSet::builder()
                .x2apic_capable(x2apic_capable)
                .hyperv_enlightenments(config.hv_config.is_some())
                .build();
            LocalApicKind::Emulated(lapic)
        } else {
            LocalApicKind::Offloaded
        };
        #[cfg(guest_arch = "aarch64")]
        let lapic = LocalApicKind::Offloaded;

        let mut whp_config = whp::PartitionConfig::new().for_op("create partition")?;

        whp_config
            .set_property(whp::PartitionProperty::ProcessorCount(
                config.processor_topology.vp_count(),
            ))
            .for_op("set processor count")?;

        #[cfg(guest_arch = "x86_64")]
        {
            use vm_topology::processor::x86::ApicMode;

            let apic_mode = if !user_mode_apic {
                match config.processor_topology.apic_mode() {
                    ApicMode::XApic => whp::abi::WHvX64LocalApicEmulationModeXApic,
                    ApicMode::X2ApicSupported | ApicMode::X2ApicEnabled => {
                        whp::abi::WHvX64LocalApicEmulationModeX2Apic
                    }
                }
            } else {
                whp::abi::WHvX64LocalApicEmulationModeNone
            };

            whp_config
                .set_property(whp::PartitionProperty::LocalApicEmulationMode(apic_mode))
                .for_op("set apic emulation mode")?;

            extended_exits |= whp::abi::WHV_EXTENDED_VM_EXITS::X64MsrExit;
            if user_mode_apic {
                whp_config
                    .set_property(whp::PartitionProperty::X64MsrExitBitmap(
                        whp::abi::WHV_X64_MSR_EXIT_BITMAP::ApicBaseMsrWrite
                            | whp::abi::WHV_X64_MSR_EXIT_BITMAP::UnhandledMsrs,
                    ))
                    .for_op("set msr exit bitmap")?;
                // Enable #GP faults to get synic MSR accesses, for which which the
                // hypervisor incorrectly fails to exit to the parent.
                extended_exits |= whp::abi::WHV_EXTENDED_VM_EXITS::ExceptionExit;
                whp_config
                    .set_property(whp::PartitionProperty::ExceptionExitBitmap(
                        1 << x86defs::Exception::GENERAL_PROTECTION_FAULT.0,
                    ))
                    .for_op("set exception exit bitmap")?;
            }
        }

        #[cfg(all(guest_arch = "aarch64", feature = "unstable_whp"))]
        {
            let gic_params = whp::abi::WHV_ARM64_IC_PARAMETERS {
                EmulationMode: whp::abi::WHV_ARM64_IC_EMULATION_MODE::GicV3,
                Reserved: 0,
                // TODO: Make all of these values configurable.
                // Using legacy Hyper-V defaults for now.
                GicV3Parameters: whp::abi::WHV_ARM64_IC_GIC_V3_PARAMETERS {
                    GicdBaseAddress: config.processor_topology.gic_distributor_base(),
                    GitsTranslatorBaseAddress: 0,
                    Reserved: 0,
                    GicLpiIntIdBits: 1,
                    GicPpiOverflowInterruptFromCntv: 0x14,
                    GicPpiPerformanceMonitorsInterrupt: 0x17,
                    Reserved1: [0; 6],
                },
            };
            whp_config
                .set_property(whp::PartitionProperty::GicParameters(gic_params))
                .for_op("set gic parameters")?;
        }

        // Request GPA access fault exits here because WHP tries to handle these
        // for ROM regions, resulting in an extra syscall and C++ exception for
        // each such exit. We know locally whether memory is supposed to be
        // mapped writable, so we can avoid this.
        // TODO-aarch64
        if cfg!(guest_arch = "x86_64") {
            extended_exits |= whp::abi::WHV_EXTENDED_VM_EXITS::GpaAccessFaultExit;
        }

        let mut with_overlays = false;
        if let Some(hv_config) = &config.hv_config {
            extended_exits |= whp::abi::WHV_EXTENDED_VM_EXITS::HypercallExit;
            #[cfg(guest_arch = "x86_64")]
            {
                extended_exits |= whp::abi::WHV_EXTENDED_VM_EXITS::X64CpuidExit;
            }

            let supported_synth_features = whp::capabilities::synthetic_processor_features()
                .for_op("get synth processor features")?;
            if hv_config.offload_enlightenments
                && !user_mode_apic
                && supported_synth_features
                    .bank0
                    .is_set(whp::abi::WHV_SYNTHETIC_PROCESSOR_FEATURES::HypervisorPresent)
            {
                hypervisor_enlightened = true;

                // TODO-aarch64: hypervisor bug
                #[cfg(guest_arch = "x86_64")]
                {
                    use whp::abi::WHV_EXTENDED_VM_EXITS as E;
                    extended_exits |= E::UnknownSynicConnection | E::RetargetUnknownVpciDevice;
                }

                let synth_features: whp::SyntheticProcessorFeatures = {
                    use whp::abi::WHV_SYNTHETIC_PROCESSOR_FEATURES as F;

                    let mut features = whp::SyntheticProcessorFeatures::default();
                    features.bank0 = F::HypervisorPresent
                        | F::Hv1
                        | F::AccessVpRunTimeReg
                        | F::AccessPartitionReferenceCounter
                        | F::AccessHypercallRegs
                        | F::AccessVpIndex
                        | F::AccessPartitionReferenceTsc
                        | F::AccessSynicRegs
                        | F::AccessSyntheticTimerRegs
                        | F::FastHypercallOutput
                        | F::ExtendedProcessorMasks
                        | F::SyntheticClusterIpi
                        | F::NotifyLongSpinWait
                        | F::QueryNumaDistance
                        | F::SignalEvents
                        | F::RetargetDeviceInterrupt;

                    #[cfg(guest_arch = "x86_64")]
                    {
                        features.bank0 |= F::TbFlushHypercalls
                            | F::AccessGuestIdleReg
                            | F::AccessFrequencyRegs
                            | F::EnableExtendedGvaRangesForFlushVirtualAddressList;
                    }

                    #[cfg(guest_arch = "aarch64")]
                    {
                        features.bank0 |= F::AccessVpRegs | F::SyncContext;
                    }

                    #[cfg(all(guest_arch = "aarch64", feature = "unstable_whp"))]
                    {
                        features.bank0 |= F::TbFlushHypercalls;
                    }

                    if vtl == Vtl::Vtl0 {
                        // We need to emulate the VP assist page for VTL2, so don't opt into the enlightenment.
                        features.bank0 |= F::AccessIntrCtrlRegs;

                        // BUG: this feature is required for running VTL2 w/ vmbus
                        // under OpenVMM to avoid timer/vmbus sint contention
                        features.bank0 |= F::DirectSyntheticTimers;
                    }

                    // Enable overlay emulation for offloading only if vtl2 is not present.
                    if hv_config.vtl2.is_none() {
                        with_overlays = true;
                    }

                    features
                };

                whp_config
                    .set_property(whp::PartitionProperty::SyntheticProcessorFeatures(
                        synth_features,
                    ))
                    .for_op("set synthetic processor features")?;
            } else {
                with_overlays = hv_config.vtl2.is_none();
            }

            if hv_config.allow_device_assignment {
                whp_config
                    .set_property(whp::PartitionProperty::AllowDeviceAssignment(true))
                    .for_op("allow device assignment")?;
            }
        }

        whp_config
            .set_property(whp::PartitionProperty::ExtendedVmExits(extended_exits))
            .for_op("set extended vm exits")?;

        let whp = whp_config.create().for_op("set up partition")?;

        for vp in config.processor_topology.vps() {
            let index = vp.vp_index.index();
            whp.create_vp(index).create().for_op("create vp")?;
        }

        let vplcs = config
            .processor_topology
            .vps()
            .map(|_| Vplc::new())
            .collect();

        #[cfg(guest_arch = "x86_64")]
        let apic_id_map = config
            .processor_topology
            .vps_arch()
            .map(|vp| vp.apic_id)
            .collect();

        let mapper: Box<dyn MemoryMapper> = if config.isolation.is_isolated() {
            // VTL2 late map support is ignored, since memory acceptance
            // requires explicit calls from the guest in order to access
            // memory.

            assert!(!with_overlays);

            match config.isolation {
                IsolationType::Vbs => {}
                ty => unimplemented!("isolation type unsupported: {ty:?}"),
            }

            Box::new(memory::vtl2_mapper::VtlMemoryMapper::new(
                MappingState::EmulatedIsolation {
                    current_vis: RangeMap::new(),
                    mapped_ranges: RangeMap::new(),
                },
            ))
        } else if let Some(vtl_config) = config
            .hv_config
            .as_ref()
            .and_then(|hv_config| hv_config.vtl2.as_ref())
        {
            assert!(!with_overlays);

            let mapping_state = if vtl == Vtl::Vtl2 && vtl_config.late_map_vtl0_memory.is_some() {
                MappingState::Deferred {
                    allowed_ranges: vec![],
                    deferred: Vec::new(),
                    mapped_ranges: RangeMap::new(),
                }
            } else {
                MappingState::Mapped {
                    reset_state: ResetMappingState::Mapped,
                    mapped_ranges: RangeMap::new(),
                }
            };

            Box::new(memory::vtl2_mapper::VtlMemoryMapper::new(mapping_state))
        } else {
            Box::new(memory::WhpMemoryMapper::new(with_overlays))
        };

        Ok(Self {
            whp,
            vplcs,
            #[cfg(guest_arch = "x86_64")]
            software_devices: virt::x86::apic_software_device::ApicSoftwareDevices::new(
                apic_id_map,
            ),
            ranges: Default::default(),
            mapper,
            lapic,
            hypervisor_enlightened,
        })
    }

    /// Reset this partition back into the state before starting VPs.
    fn reset(&self) -> Result<(), Error> {
        self.whp.reset().for_op("reset partition")?;
        self.reset_mappings().map_err(Error::ResetMemoryMapping)?;
        Ok(())
    }
}

struct WhpNoVtlProtections<'a>(&'a GuestMemory);
impl<'a> hv1_emulator::VtlProtectAccess for WhpNoVtlProtections<'a> {
    fn check_modify_and_lock_overlay_page(
        &mut self,
        gpn: u64,
        _check_perms: hvdef::HvMapGpaFlags,
        _new_perms: Option<hvdef::HvMapGpaFlags>,
    ) -> Result<guestmem::LockedPages, hvdef::HvError> {
        Ok(self.0.lock_gpns(false, &[gpn]).unwrap())
    }

    fn unlock_overlay_page(&mut self, _gpn: u64) -> Result<(), hvdef::HvError> {
        Ok(())
    }
}

impl Hv1State {
    fn reset(&self, guest_memory: &GuestMemory) {
        match self {
            Hv1State::Emulated(hv) => hv.reset(
                [
                    &mut WhpNoVtlProtections(guest_memory)
                        as &mut dyn hv1_emulator::VtlProtectAccess,
                    &mut WhpNoVtlProtections(guest_memory),
                    &mut WhpNoVtlProtections(guest_memory),
                ]
                .into(),
            ),
            Hv1State::Offloaded => {}
            Hv1State::Disabled => {}
        }
    }
}

impl Drop for WhpProcessor<'_> {
    fn drop(&mut self) {
        // Remove the waker to avoid keeping a reference to the VP run task.
        let _waker = self.inner.waker.write().take();
    }
}

impl<'p> virt::Processor for WhpProcessor<'p> {
    type StateAccess<'a>
        = WhpVpStateAccess<'a, 'p>
    where
        Self: 'a;

    fn set_debug_state(
        &mut self,
        _vtl: Vtl,
        _state: Option<&virt::x86::DebugState>,
    ) -> Result<(), <WhpVpStateAccess<'_, 'p> as virt::vp::AccessVpState>::Error> {
        Err(Error::GuestDebuggingNotSupported)
    }

    fn vtl_inspectable(&self, vtl: Vtl) -> bool {
        self.state.enabled_vtls.is_set(vtl)
    }

    // We guarantee elsewhere that there will be no concurrent calls to this function. Therefore it is ok to hold the
    // lock across an await point, as no lock contention means no possibility of deadlocks or long waits.
    async fn run_vp(
        &mut self,
        stop: StopVp<'_>,
        dev: &impl CpuIo,
    ) -> Result<Infallible, VpHaltReason> {
        self.run_vp(stop, dev).await
    }

    fn flush_async_requests(&mut self) {
        // TODO: flush more (e.g. HvStartVp context)
        self.flush_apic(Vtl::Vtl0);
        if self.state.vtls.vtl2.is_some() {
            self.flush_apic(Vtl::Vtl2);
        }
    }

    fn access_state(&mut self, vtl: Vtl) -> Self::StateAccess<'_> {
        self.access_state(vtl)
    }
}

impl virt::Hv1 for WhpPartition {
    type Error = Error;
    #[cfg(guest_arch = "x86_64")]
    type Device = virt::x86::apic_software_device::ApicSoftwareDevice;
    #[cfg(guest_arch = "aarch64")]
    type Device = virt::aarch64::gic_software_device::GicSoftwareDevice;

    fn reference_time_source(&self) -> Option<ReferenceTimeSource> {
        match &self.inner.hvstate {
            Hv1State::Emulated(hv) => Some(hv.ref_time_source().clone()),
            Hv1State::Offloaded => Some(ReferenceTimeSource::from(self.inner.clone() as Arc<_>)),
            Hv1State::Disabled => None,
        }
    }

    fn new_virtual_device(
        &self,
    ) -> Option<&dyn virt::DeviceBuilder<Device = Self::Device, Error = Self::Error>> {
        Some(self)
    }
}

impl GetReferenceTime for WhpPartitionInner {
    fn now(&self) -> ReferenceTimeResult {
        ReferenceTimeResult {
            ref_time: self.vtl0.whp.reference_time().unwrap(),
            system_time: None,
        }
    }
}

impl WhpPartition {
    pub fn new_physical_device(
        &self,
        vtl: Vtl,
        device_id: u64,
        resource: whp::VpciResource,
    ) -> Result<device::Device, Error> {
        assert_eq!(
            vtl,
            Vtl::Vtl0,
            "cannot assign devices to VTL2 since there's no way to switch VTLs at interrupt time"
        );
        device::Device::new_physical(self.inner.clone(), vtl, device_id, resource)
            .for_op("assign physical device")
    }

    // Validate that everything is in the reset state.
    fn validate_is_reset(&self, vtl: Vtl) {
        if cfg!(debug_assertions) {
            self.access_state(vtl)
                .check_reset_all(&self.inner.bsp().vp().vp_info);
        }
    }

    fn with_vtl(&self, vtl: Vtl) -> &Arc<WhpPartitionAndVtl> {
        match vtl {
            Vtl::Vtl0 => &self.with_vtl0,
            Vtl::Vtl2 => self.with_vtl2.as_ref().unwrap(),
            _ => unimplemented!(),
        }
    }
}

#[derive(Clone)]
struct WhpPartitionAndVtl {
    partition: Arc<WhpPartitionInner>,
    vtl: Vtl,
}

impl WhpPartitionAndVtl {
    fn vtlp(&self) -> &VtlPartition {
        self.partition.vtlp(self.vtl)
    }
}

#[cfg(guest_arch = "x86_64")]
mod x86 {
    use crate::Error;
    use crate::LocalApicKind;
    use crate::WhpPartition;
    use crate::WhpPartitionInner;
    use hvdef::Vtl;
    use virt::VpIndex;
    use virt::irqcon::MsiRequest;

    impl WhpPartitionInner {
        pub(crate) fn synic_interrupt(
            &self,
            vp: VpIndex,
            vtl: Vtl,
        ) -> impl '_ + hv1_emulator::RequestInterrupt {
            move |vec: u32, auto_eoi| match &self.vtlp(vtl).lapic {
                LocalApicKind::Emulated(lapic) => {
                    lapic.synic_interrupt(vp, vec as u8, auto_eoi, |vp_index| {
                        self.vp(vp_index)
                            .expect("apic emulator passes valid vp index")
                            .wake()
                    });
                }
                LocalApicKind::Offloaded => unreachable!(),
            }
        }
    }

    impl virt::DeviceBuilder for WhpPartition {
        fn build(&self, vtl: Vtl, device_id: u64) -> Result<Self::Device, Self::Error> {
            self.inner
                .vtlp(vtl)
                .software_devices
                .new_device(self.with_vtl(vtl).clone(), device_id)
                .map_err(Error::NewDevice)
        }
    }

    impl virt::irqcon::IoApicRouting for WhpPartitionInner {
        fn set_irq_route(&self, irq: u8, request: Option<MsiRequest>) {
            self.irq_routes.set_irq_route(irq, request)
        }

        fn assert_irq(&self, irq: u8) {
            self.irq_routes.assert_irq(irq, |request| {
                if let Err(err) = self.interrupt(Vtl::Vtl0, request) {
                    tracelimit::warn_ratelimited!(
                        address = request.address,
                        data = request.data,
                        error = &err as &dyn std::error::Error,
                        "failed to request io-apic interrupt"
                    );
                }
            });
        }
    }
}

#[cfg(guest_arch = "aarch64")]
mod aarch64 {
    use crate::Error;
    use crate::WhpPartition;
    use crate::WhpPartitionAndVtl;
    use crate::WhpPartitionInner;
    use hvdef::Vtl;
    use virt::VpIndex;
    use virt::irqcon::MsiRequest;

    /// On aarch64, the platform configured GSIV value for the PMU.
    pub const WHP_PMU_GSIV: u32 = 0x17;

    impl WhpPartitionInner {
        pub(crate) fn synic_interrupt(
            &self,
            vp: VpIndex,
            vtl: Vtl,
        ) -> impl hv1_emulator::RequestInterrupt + use<> {
            let _ = (vp, vtl);
            move |_vec, _auto_eoi| todo!("TODO-aarch64")
        }
    }

    impl virt::DeviceBuilder for WhpPartition {
        fn build(&self, vtl: Vtl, device_id: u64) -> Result<Self::Device, Self::Error> {
            let _ = device_id;
            Ok(virt::aarch64::gic_software_device::GicSoftwareDevice::new(
                self.with_vtl(vtl).clone(),
            ))
        }
    }

    impl WhpPartitionInner {
        pub(crate) fn interrupt(&self, _vtl: Vtl, _request: MsiRequest) -> Result<(), Error> {
            tracelimit::warn_ratelimited!("msis not supported");
            Ok(())
        }
    }

    impl virt::irqcon::ControlGic for WhpPartitionAndVtl {
        fn set_spi_irq(&self, irq_id: u32, high: bool) {
            if let Err(err) = self.vtlp().whp.interrupt(irq_id, high) {
                tracelimit::warn_ratelimited!(
                    irq_id,
                    high,
                    error = &err as &dyn std::error::Error,
                    "failed to set interrupt state"
                );
            }
        }
    }
}
