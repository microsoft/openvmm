// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module contains Underhill specific functionality and implementations of require traits
//! in order to plug into the rest of the common HvLite code.

pub mod mshv;
mod nice;
mod vp_state;

cfg_if::cfg_if! {
    if #[cfg(guest_arch = "x86_64")] {
        mod hardware_cvm;
        pub mod snp;
        pub mod tdx;

        use crate::TlbFlushLockAccess;
        use crate::VtlCrash;
        use bitvec::prelude::BitArray;
        use bitvec::prelude::Lsb0;
        use hv1_emulator::synic::ProcessorSynic;
        use hvdef::HvRegisterCrInterceptControl;
        use hvdef::HvX64RegisterName;
        use virt::vp::MpState;
        use virt::x86::MsrError;
        use virt_support_apic::LocalApic;
        use virt_support_x86emu::translate::TranslationRegisters;
        use virt::vp::AccessVpState;
        use zerocopy::IntoBytes;
    } else if #[cfg(guest_arch = "aarch64")] {
        use hv1_hypercall::Arm64RegisterState;
        use hvdef::HvArm64RegisterName;
    } else {
        compile_error!("unsupported guest architecture");
    }
}

use super::Error;
use super::UhPartitionInner;
use super::UhVpInner;
use crate::ExitActivity;
use crate::GuestVtl;
use crate::WakeReason;
use cvm_tracing::CVM_ALLOWED;
use cvm_tracing::CVM_CONFIDENTIAL;
use hcl::ioctl::Hcl;
use hcl::ioctl::ProcessorRunner;
use hv1_emulator::message_queues::MessageQueues;
use hv1_hypercall::HvRepResult;
use hv1_structs::ProcessorSet;
use hv1_structs::VtlArray;
use hvdef::HvError;
use hvdef::HvMessage;
use hvdef::HvSynicSint;
use hvdef::NUM_SINTS;
use hvdef::Vtl;
use inspect::Inspect;
use inspect::InspectMut;
use pal::unix::affinity;
use pal::unix::affinity::CpuSet;
use pal_async::driver::Driver;
use pal_async::driver::PollImpl;
use pal_async::timer::PollTimer;
use pal_uring::IdleControl;
use private::BackingPrivate;
use std::convert::Infallible;
use std::future::poll_fn;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::Poll;
use virt::EmulatorMonitorSupport;
use virt::Processor;
use virt::StopVp;
use virt::VpHaltReason;
use virt::VpIndex;
use virt::io::CpuIo;
use vm_topology::processor::TargetVpInfo;
use vmcore::vmtime::VmTimeAccess;

/// An object to run lower VTLs and to access processor state.
///
/// This is not [`Send`] and can only be instantiated from
/// [`crate::UhProcessorBox::bind_processor`]. This ensures that it can only be used
/// from a thread that is affinitized to the VP, since it is only possible to
/// access lower VTL processor state from the same processor.
#[derive(InspectMut)]
#[inspect(extra = "UhProcessor::inspect_extra", bound = "T: Backing")]
pub struct UhProcessor<'a, T: Backing> {
    _not_send: PhantomData<*mut ()>,

    #[inspect(flatten)]
    inner: &'a UhVpInner,
    #[inspect(skip)]
    partition: &'a UhPartitionInner,
    #[inspect(skip)]
    idle_control: Option<&'a mut IdleControl>,
    #[inspect(skip)]
    kernel_returns: u64,
    #[inspect(hex, iter_by_index)]
    crash_reg: [u64; hvdef::HV_X64_GUEST_CRASH_PARAMETER_MSRS],
    vmtime: VmTimeAccess,
    #[inspect(skip)]
    timer: PollImpl<dyn PollTimer>,
    #[inspect(mut)]
    force_exit_sidecar: bool,
    signaled_sidecar_exit: bool,
    /// The VTLs on this VP that are currently locked, per requesting VTL.
    vtls_tlb_locked: VtlsTlbLocked,
    #[inspect(skip)]
    shared: &'a T::Shared,
    #[inspect(hex, with = "|x| inspect::iter_by_index(x.iter()).map_value(|a| a.0)")]
    exit_activities: VtlArray<ExitActivity, 2>,

    // Put the runner and backing at the end so that monomorphisms of functions
    // that don't access backing-specific state are more likely to be folded
    // together by the compiler.
    #[inspect(skip)]
    runner: ProcessorRunner<'a, T::HclBacking<'a>>,
    #[inspect(mut)]
    backing: T,
}

#[derive(Inspect)]
struct VtlsTlbLocked {
    // vtl0: VtlArray<bool, 0>,
    vtl1: VtlArray<bool, 1>,
    vtl2: VtlArray<bool, 2>,
}

impl VtlsTlbLocked {
    fn get(&self, requesting_vtl: Vtl, target_vtl: GuestVtl) -> bool {
        match requesting_vtl {
            Vtl::Vtl0 => unreachable!(),
            Vtl::Vtl1 => self.vtl1[target_vtl],
            Vtl::Vtl2 => self.vtl2[target_vtl],
        }
    }

    fn set(&mut self, requesting_vtl: Vtl, target_vtl: GuestVtl, value: bool) {
        match requesting_vtl {
            Vtl::Vtl0 => unreachable!(),
            Vtl::Vtl1 => self.vtl1[target_vtl] = value,
            Vtl::Vtl2 => self.vtl2[target_vtl] = value,
        }
    }

    fn fill(&mut self, requesting_vtl: Vtl, value: bool) {
        match requesting_vtl {
            Vtl::Vtl0 => unreachable!(),
            Vtl::Vtl1 => self.vtl1.fill(value),
            Vtl::Vtl2 => self.vtl2.fill(value),
        }
    }
}

#[cfg(guest_arch = "x86_64")]
#[derive(Inspect)]
pub(crate) struct LapicState {
    lapic: LocalApic,
    activity: MpState,
    nmi_pending: bool,
}

#[cfg(guest_arch = "x86_64")]
impl LapicState {
    pub fn new(lapic: LocalApic, activity: MpState) -> Self {
        Self {
            lapic,
            activity,
            nmi_pending: false,
        }
    }
}

struct BackingParams<'a, 'b, T: Backing> {
    partition: &'a UhPartitionInner,
    vp_info: &'a TargetVpInfo,
    runner: &'a mut ProcessorRunner<'b, T::HclBacking<'b>>,
}

mod private {
    use super::BackingParams;
    use super::vp_state;
    use crate::BackingShared;
    use crate::Error;
    use crate::GuestVtl;
    use crate::processor::UhProcessor;
    use hv1_emulator::hv::ProcessorVtlHv;
    use hv1_structs::VtlArray;
    use inspect::InspectMut;
    use std::future::Future;
    use virt::StopVp;
    use virt::VpHaltReason;
    use virt::io::CpuIo;
    use virt::vp::AccessVpState;

    #[expect(private_interfaces)]
    pub trait BackingPrivate: 'static + Sized + InspectMut + Sized {
        type HclBacking<'b>: hcl::ioctl::Backing<'b>;
        type EmulationCache;
        type Shared;

        fn shared(shared: &BackingShared) -> &Self::Shared;

        fn new(params: BackingParams<'_, '_, Self>, shared: &Self::Shared) -> Result<Self, Error>;

        type StateAccess<'p, 'a>: AccessVpState<Error = vp_state::Error>
        where
            Self: 'a + 'p,
            'p: 'a;

        fn init(this: &mut UhProcessor<'_, Self>);

        fn access_vp_state<'a, 'p>(
            this: &'a mut UhProcessor<'p, Self>,
            vtl: GuestVtl,
        ) -> Self::StateAccess<'p, 'a>;

        /// Called before the first `run_vp` call to allow the backing to
        /// perform any pre-run tasks. Must be idempotent--may be called again
        /// before some subsequent `run_vp` calls.
        fn pre_run_vp(_this: &mut UhProcessor<'_, Self>) {}

        fn run_vp(
            this: &mut UhProcessor<'_, Self>,
            dev: &impl CpuIo,
            stop: &mut StopVp<'_>,
        ) -> impl Future<Output = Result<(), VpHaltReason>>;

        /// Process any pending interrupts. Returns true if reprocessing is required.
        fn process_interrupts(
            this: &mut UhProcessor<'_, Self>,
            scan_irr: VtlArray<bool, 2>,
            first_scan_irr: &mut bool,
            dev: &impl CpuIo,
        ) -> bool;

        /// Process any pending APIC work.
        fn poll_apic(this: &mut UhProcessor<'_, Self>, vtl: GuestVtl, scan_irr: bool);

        /// Requests the VP to exit when an external interrupt is ready to be
        /// delivered.
        ///
        /// Only used when the hypervisor implements the APIC.
        fn request_extint_readiness(this: &mut UhProcessor<'_, Self>);

        /// Requests the VP to exit when any of the specified SINTs have a free
        /// message slot.
        ///
        /// This is used for hypervisor-managed and untrusted SINTs.
        fn request_untrusted_sint_readiness(this: &mut UhProcessor<'_, Self>, sints: u16);

        fn handle_vp_start_enable_vtl_wake(_this: &mut UhProcessor<'_, Self>, _vtl: GuestVtl);

        fn inspect_extra(_this: &mut UhProcessor<'_, Self>, _resp: &mut inspect::Response<'_>) {}

        fn hv(&self, vtl: GuestVtl) -> Option<&ProcessorVtlHv>;
        fn hv_mut(&mut self, vtl: GuestVtl) -> Option<&mut ProcessorVtlHv>;

        fn vtl1_inspectable(this: &UhProcessor<'_, Self>) -> bool;
    }
}

/// Processor backing.
pub trait Backing: BackingPrivate {}

impl<T: BackingPrivate> Backing for T {}

#[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
pub(crate) struct BackingSharedParams<'a> {
    pub cvm_state: Option<crate::UhCvmPartitionState>,
    #[cfg(guest_arch = "x86_64")]
    pub cpuid: &'a virt::CpuidLeafSet,
    pub hcl: &'a Hcl,
    pub guest_vsm_available: bool,
    pub lower_vtl_timer_virt_available: bool,
}

/// Supported intercept message types.
#[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
enum InterceptMessageType {
    #[cfg(guest_arch = "x86_64")]
    Register {
        reg: HvX64RegisterName,
        value: u64,
    },
    Msr {
        msr: u32,
    },
    #[cfg(guest_arch = "x86_64")]
    IoPort {
        port_number: u16,
        access_size: u8,
        string_access: bool,
        rep_access: bool,
    },
}

/// Per-arch state required to generate an intercept message.
#[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
pub(crate) struct InterceptMessageState {
    instruction_length_and_cr8: u8,
    cpl: u8,
    efer_lma: bool,
    cs: hvdef::HvX64SegmentRegister,
    rip: u64,
    rflags: u64,
    rax: u64,
    rdx: u64,
    rcx: u64,
    rsi: u64,
    rdi: u64,
    optional: Option<InterceptMessageOptionalState>,
}

#[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
/// Additional per-arch state required to generate an intercept message. Used
/// for state that is not common across the intercept message types and that
/// might be slower to retrieve on certain architectures.
struct InterceptMessageOptionalState {
    ds: hvdef::HvX64SegmentRegister,
    es: hvdef::HvX64SegmentRegister,
}

impl InterceptMessageType {
    #[cfg(guest_arch = "x86_64")]
    fn generate_hv_message(
        &self,
        vp_index: VpIndex,
        vtl: GuestVtl,
        state: InterceptMessageState,
        is_read: bool,
    ) -> HvMessage {
        let header = hvdef::HvX64InterceptMessageHeader {
            vp_index: vp_index.index(),
            instruction_length_and_cr8: state.instruction_length_and_cr8,
            intercept_access_type: if is_read {
                hvdef::HvInterceptAccessType::READ
            } else {
                hvdef::HvInterceptAccessType::WRITE
            },
            execution_state: hvdef::HvX64VpExecutionState::new()
                .with_cpl(state.cpl)
                .with_vtl(vtl.into())
                .with_efer_lma(state.efer_lma),
            cs_segment: state.cs,
            rip: state.rip,
            rflags: state.rflags,
        };
        match self {
            InterceptMessageType::Register { reg, value } => {
                let intercept_message = hvdef::HvX64RegisterInterceptMessage {
                    header,
                    flags: hvdef::HvX64RegisterInterceptMessageFlags::new(),
                    rsvd: 0,
                    rsvd2: 0,
                    register_name: *reg,
                    access_info: hvdef::HvX64RegisterAccessInfo::new_source_value(
                        hvdef::HvRegisterValue::from(*value),
                    ),
                };
                HvMessage::new(
                    hvdef::HvMessageType::HvMessageTypeRegisterIntercept,
                    0,
                    intercept_message.as_bytes(),
                )
            }
            InterceptMessageType::Msr { msr } => {
                let intercept_message = hvdef::HvX64MsrInterceptMessage {
                    header,
                    msr_number: *msr,
                    rax: state.rax,
                    rdx: state.rdx,
                    reserved: 0,
                };

                HvMessage::new(
                    hvdef::HvMessageType::HvMessageTypeMsrIntercept,
                    0,
                    intercept_message.as_bytes(),
                )
            }
            InterceptMessageType::IoPort {
                port_number,
                access_size,
                string_access,
                rep_access,
            } => {
                let access_info =
                    hvdef::HvX64IoPortAccessInfo::new(*access_size, *string_access, *rep_access);
                let intercept_message = hvdef::HvX64IoPortInterceptMessage {
                    header,
                    port_number: *port_number,
                    access_info,
                    instruction_byte_count: 0,
                    reserved: 0,
                    rax: state.rax,
                    instruction_bytes: [0u8; 16],
                    ds_segment: state.optional.as_ref().unwrap().ds,
                    es_segment: state.optional.as_ref().unwrap().es,
                    rcx: state.rcx,
                    rsi: state.rsi,
                    rdi: state.rdi,
                };

                HvMessage::new(
                    hvdef::HvMessageType::HvMessageTypeX64IoPortIntercept,
                    0,
                    intercept_message.as_bytes(),
                )
            }
        }
    }
}

/// Trait for processor backings that have hardware isolation support.
#[cfg(guest_arch = "x86_64")]
pub(crate) trait HardwareIsolatedBacking: Backing {
    /// Gets CVM specific VP state.
    fn cvm_state(&self) -> &crate::UhCvmVpState;
    /// Gets CVM specific VP state.
    fn cvm_state_mut(&mut self) -> &mut crate::UhCvmVpState;
    /// Gets CVM specific partition state.
    fn cvm_partition_state(shared: &Self::Shared) -> &crate::UhCvmPartitionState;
    /// Gets a struct that can be used to interact with TLB flushing and
    /// locking.
    ///
    /// If a `vp_index` is provided, it will be used to cause the specified VP
    /// to wait for all TLB locks to be released before returning to a lower VTL.
    //
    // FUTURE: This probably shouldn't be optional, but right now MNF doesn't know
    // what VP it's set from and probably doesn't need it?
    fn tlb_flush_lock_access<'a>(
        vp_index: Option<VpIndex>,
        partition: &'a UhPartitionInner,
        shared: &'a Self::Shared,
    ) -> impl TlbFlushLockAccess + 'a;
    /// Copies shared registers (per VSM TLFS spec) from the source VTL to
    /// the target VTL that will become active, and set the exit vtl
    fn switch_vtl(this: &mut UhProcessor<'_, Self>, source_vtl: GuestVtl, target_vtl: GuestVtl);
    /// Gets registers needed for gva to gpa translation
    fn translation_registers(
        &self,
        this: &UhProcessor<'_, Self>,
        vtl: GuestVtl,
    ) -> TranslationRegisters;
    /// Vector of the event that is pending injection into the guest state, if
    /// valid.
    fn pending_event_vector(this: &UhProcessor<'_, Self>, vtl: GuestVtl) -> Option<u8>;
    /// Check if an interrupt of appropriate priority, or an NMI, is pending for
    /// the given VTL. `check_rflags` specifies whether RFLAGS.IF should be checked.
    fn is_interrupt_pending(
        this: &mut UhProcessor<'_, Self>,
        vtl: GuestVtl,
        check_rflags: bool,
        dev: &impl CpuIo,
    ) -> bool;
    /// Sets the pending exception for the guest state.
    ///
    /// Note that this will overwrite any existing pending exception. It will
    /// not handle merging any existing pending exception with the new one.
    fn set_pending_exception(
        this: &mut UhProcessor<'_, Self>,
        vtl: GuestVtl,
        event: hvdef::HvX64PendingExceptionEvent,
    );

    fn intercept_message_state(
        this: &UhProcessor<'_, Self>,
        vtl: GuestVtl,
        include_optional_state: bool,
    ) -> InterceptMessageState;

    /// Individual register for CPUID and crx intercept handling, since
    /// AccessVpState::registers is relatively slow on TDX.
    fn cr0(this: &UhProcessor<'_, Self>, vtl: GuestVtl) -> u64;
    fn cr4(this: &UhProcessor<'_, Self>, vtl: GuestVtl) -> u64;

    fn cr_intercept_registration(
        this: &mut UhProcessor<'_, Self>,
        intercept_control: HvRegisterCrInterceptControl,
    );

    fn untrusted_synic_mut(&mut self) -> Option<&mut ProcessorSynic>;

    /// Updates the timer deadline.
    fn update_deadline(this: &mut UhProcessor<'_, Self>, ref_time_now: u64, next_ref_time: u64);

    /// Clears any pending timer deadline.
    fn clear_deadline(this: &mut UhProcessor<'_, Self>);
}

#[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
#[derive(Inspect, Debug)]
#[inspect(tag = "reason")]
pub(crate) enum SidecarExitReason {
    #[inspect(transparent)]
    Exit(SidecarRemoveExit),
    #[inspect(transparent)]
    TaskRequest(Arc<str>),
    ManualRequest,
}

#[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
#[derive(Inspect, Debug)]
#[inspect(tag = "exit")]
pub(crate) enum SidecarRemoveExit {
    Msr {
        #[inspect(hex)]
        msr: u32,
        value: Option<u64>,
    },
    Io {
        #[inspect(hex)]
        port: u16,
        write: bool,
    },
    Mmio {
        #[inspect(hex)]
        gpa: u64,
        write: bool,
    },
    Hypercall {
        #[inspect(debug)]
        code: hvdef::HypercallCode,
    },
    Cpuid {
        #[inspect(hex)]
        leaf: u32,
        #[inspect(hex)]
        subleaf: u32,
    },
    Hypervisor {
        #[inspect(debug)]
        message: hvdef::HvMessageType,
    },
}

impl UhVpInner {
    // Create a new vp's state.
    pub fn new(cpu_index: u32, vp_info: TargetVpInfo) -> Self {
        Self {
            wake_reasons: Default::default(),
            message_queues: VtlArray::from_fn(|_| MessageQueues::new()),
            waker: Default::default(),
            cpu_index,
            vp_info,
            sidecar_exit_reason: Default::default(),
        }
    }

    /// Queues a message for sending, optionally alerting the hypervisor if the queue is empty.
    pub fn post_message(&self, vtl: GuestVtl, sint: u8, message: &HvMessage) {
        if self.message_queues[vtl].enqueue_message(sint, message) {
            self.wake(vtl, WakeReason::MESSAGE_QUEUES);
        }
    }

    pub fn wake(&self, vtl: GuestVtl, reason: WakeReason) {
        let reason = u64::from(reason.0) << (vtl as u8 * 32);
        if self.wake_reasons.fetch_or(reason, Ordering::Release) & reason == 0 {
            if let Some(waker) = &*self.waker.read() {
                waker.wake_by_ref();
            }
        }
    }

    pub fn wake_vtl2(&self) {
        if let Some(waker) = &*self.waker.read() {
            waker.wake_by_ref();
        }
    }

    pub fn set_sidecar_exit_reason(&self, reason: SidecarExitReason) {
        self.sidecar_exit_reason.lock().get_or_insert_with(|| {
            tracing::info!(CVM_ALLOWED, "sidecar exit");
            tracing::info!(CVM_CONFIDENTIAL, ?reason, "sidecar exit");
            reason
        });
    }
}

impl<T: Backing> UhProcessor<'_, T> {
    fn inspect_extra(&mut self, resp: &mut inspect::Response<'_>) {
        resp.child("stats", |req| {
            // Get all the VP stats and just grab this VP's.
            if let Ok(stats) = hcl::stats::vp_stats() {
                let stats = &stats[self.vp_index().index() as usize];
                req.respond()
                    .counter("vtl_transitions", stats.vtl_transitions)
                    .counter(
                        "spurious_exits",
                        stats.vtl_transitions.saturating_sub(self.kernel_returns),
                    );
            }
        })
        .field(
            "last_enter_modes",
            self.runner
                .enter_mode()
                .map(|&mut v| inspect::AsHex(u8::from(v))),
        )
        .field("sidecar", self.runner.is_sidecar())
        .field(
            "sidecar_base_cpu",
            self.partition.hcl.sidecar_base_cpu(self.vp_index().index()),
        );

        T::inspect_extra(self, resp);
    }

    #[cfg(guest_arch = "x86_64")]
    fn handle_debug_exception(
        &mut self,
        dev: &impl CpuIo,
        vtl: GuestVtl,
    ) -> Result<(), VpHaltReason> {
        // FUTURE: Underhill does not yet support VTL1 so this is only tested with VTL0.
        if vtl == GuestVtl::Vtl0 {
            let debug_regs: virt::x86::vp::DebugRegisters = self
                .access_state(Vtl::Vtl0)
                .debug_regs()
                .expect("register query should not fail");

            let dr = [
                debug_regs.dr0,
                debug_regs.dr1,
                debug_regs.dr2,
                debug_regs.dr3,
            ];

            if debug_regs.dr6 & x86defs::DR6_SINGLE_STEP != 0 {
                return Err(VpHaltReason::SingleStep);
            }

            // Last four bits of DR6 indicate which breakpoint was triggered.
            const BREAKPOINT_INDEX_OFFSET: usize = 4;
            let i = debug_regs.dr6.trailing_zeros() as usize;
            if i >= BREAKPOINT_INDEX_OFFSET {
                // Received a debug exception not triggered by a breakpoint or single step.
                return Err(dev.fatal_error(
                    UnexpectedDebugException {
                        dr6: debug_regs.dr6,
                    }
                    .into(),
                ));
            }
            let bp = virt::x86::HardwareBreakpoint::from_dr7(debug_regs.dr7, dr[i], i);

            return Err(VpHaltReason::HwBreak(bp));
        }

        panic!("unexpected debug exception in VTL {:?}", vtl);
    }
}

#[cfg(guest_arch = "x86_64")]
#[derive(Debug, Error)]
#[error("unexpected debug exception with dr6 value {dr6:#x}")]
struct UnexpectedDebugException {
    dr6: u64,
}

impl<'p, T: Backing> Processor for UhProcessor<'p, T> {
    type StateAccess<'a>
        = T::StateAccess<'p, 'a>
    where
        Self: 'a;

    #[cfg(guest_arch = "aarch64")]
    fn set_debug_state(
        &mut self,
        _vtl: Vtl,
        _state: Option<&virt::x86::DebugState>,
    ) -> Result<(), <T::StateAccess<'p, '_> as virt::vp::AccessVpState>::Error> {
        unimplemented!()
    }

    #[cfg(guest_arch = "x86_64")]
    fn set_debug_state(
        &mut self,
        vtl: Vtl,
        state: Option<&virt::x86::DebugState>,
    ) -> Result<(), <T::StateAccess<'p, '_> as AccessVpState>::Error> {
        // FUTURE: Underhill does not yet support VTL1 so this is only tested with VTL0.
        if vtl == Vtl::Vtl0 {
            let mut db: [u64; 4] = [0; 4];
            let mut access_state = self.access_state(vtl);
            let mut registers = access_state.registers()?;
            let mut rflags = x86defs::RFlags::from(registers.rflags);
            let mut dr7: u64 = 0;

            if let Some(state) = state {
                rflags.set_trap(state.single_step);
                for (i, bp) in state.breakpoints.iter().enumerate() {
                    if let Some(bp) = bp {
                        db[i] = bp.address;
                        dr7 |= bp.dr7_bits(i);
                    }
                }
            }

            let debug_registers = virt::x86::vp::DebugRegisters {
                dr0: db[0],
                dr1: db[1],
                dr2: db[2],
                dr3: db[3],
                dr6: 0,
                dr7,
            };
            access_state.set_debug_regs(&debug_registers)?;

            registers.rflags = rflags.into();
            access_state.set_registers(&registers)?;
            return Ok(());
        }

        panic!("unexpected set debug state in VTL {:?}", vtl);
    }

    async fn run_vp(
        &mut self,
        mut stop: StopVp<'_>,
        dev: &impl CpuIo,
    ) -> Result<Infallible, VpHaltReason> {
        T::pre_run_vp(self);

        if self.runner.is_sidecar() {
            if self.force_exit_sidecar && !self.signaled_sidecar_exit {
                self.inner
                    .set_sidecar_exit_reason(SidecarExitReason::ManualRequest);
                self.signaled_sidecar_exit = true;
                return Err(VpHaltReason::Cancel);
            }
        } else {
            let mut current = Default::default();
            affinity::get_current_thread_affinity(&mut current).unwrap();
            assert_eq!(&current, CpuSet::new().set(self.inner.cpu_index));

            // Lower the priority of this VP thread so that the VM does not return
            // to VTL0 while there is still outstanding VTL2 work to do.
            nice::nice(1);
        }

        let mut last_waker = None;

        // Force deliverability notifications to be reevaluated.
        let vtl0_wakes = WakeReason::new()
            .with_message_queues(true)
            .with_intcon(true);
        let vtl1_wakes = WakeReason::new().with_message_queues(true);
        self.inner.wake_reasons.fetch_or(
            ((vtl1_wakes.0 as u64) << 32) | (vtl0_wakes.0 as u64),
            Ordering::Relaxed,
        );

        let mut first_scan_irr = true;

        loop {
            // Process VP activity and wait for the VP to be ready.
            poll_fn(|cx| {
                loop {
                    stop.check()?;

                    // Clear the run VP cancel request.
                    self.runner.clear_cancel();

                    // Cancel any pending timer.
                    self.vmtime.cancel_timeout();

                    // Ensure the waker is set.
                    if !last_waker
                        .as_ref()
                        .is_some_and(|waker| cx.waker().will_wake(waker))
                    {
                        last_waker = Some(cx.waker().clone());
                        self.inner.waker.write().clone_from(&last_waker);
                    }

                    // Process wakes.
                    let scan_irr = if self.inner.wake_reasons.load(Ordering::Relaxed) != 0 {
                        self.handle_wake()
                    } else {
                        [false, false].into()
                    };

                    if T::process_interrupts(self, scan_irr, &mut first_scan_irr, dev) {
                        continue;
                    }

                    // Arm the timer.
                    if let Some(timeout) = self.vmtime.get_timeout() {
                        let deadline = self.vmtime.host_time(timeout);
                        if self.timer.poll_timer(cx, deadline).is_ready() {
                            continue;
                        }
                    }

                    return <Result<_, VpHaltReason>>::Ok(()).into();
                }
            })
            .await?;

            // Yield if the thread pool is not ready to block.
            if let Some(idle_control) = &mut self.idle_control {
                if !idle_control.pre_block() {
                    yield_now().await;
                    continue;
                }
            }

            if let Some(mode) = self.runner.enter_mode() {
                *mode = self
                    .partition
                    .enter_modes_atomic
                    .load(Ordering::Relaxed)
                    .into();
            }

            // Quiesce RCU before running the VP to avoid having to synchronize with
            // this CPU during memory protection updates.
            minircu::global().quiesce();

            T::run_vp(self, dev, &mut stop).await?;
            self.kernel_returns += 1;
        }
    }

    fn flush_async_requests(&mut self) {
        if self.inner.wake_reasons.load(Ordering::Relaxed) != 0 {
            let scan_irr = self.handle_wake();
            for vtl in [GuestVtl::Vtl1, GuestVtl::Vtl0] {
                if scan_irr[vtl] {
                    T::poll_apic(self, vtl, true);
                }
            }
        }
        self.runner.flush_deferred_state();
    }

    fn access_state(&mut self, vtl: Vtl) -> Self::StateAccess<'_> {
        T::access_vp_state(self, vtl.try_into().unwrap())
    }

    fn vtl_inspectable(&self, vtl: Vtl) -> bool {
        match vtl {
            Vtl::Vtl0 => true,
            Vtl::Vtl1 => T::vtl1_inspectable(self),
            Vtl::Vtl2 => false,
        }
    }
}

impl<'a, T: Backing> UhProcessor<'a, T> {
    pub(super) fn new(
        driver: &impl Driver,
        partition: &'a UhPartitionInner,
        vp_info: TargetVpInfo,
        idle_control: Option<&'a mut IdleControl>,
    ) -> Result<Self, Error> {
        let inner = partition.vp(vp_info.base.vp_index).unwrap();
        let mut runner = partition
            .hcl
            .runner(inner.vp_index().index(), idle_control.is_none())
            .unwrap();

        let backing_shared = T::shared(&partition.backing_shared);

        let backing = T::new(
            BackingParams {
                partition,
                vp_info: &vp_info,
                runner: &mut runner,
            },
            backing_shared,
        )?;

        let mut vp = Self {
            partition,
            inner,
            runner,
            idle_control,
            kernel_returns: 0,
            crash_reg: [0; hvdef::HV_X64_GUEST_CRASH_PARAMETER_MSRS],
            _not_send: PhantomData,
            backing,
            shared: backing_shared,
            vmtime: partition
                .vmtime
                .access(format!("vp-{}", vp_info.base.vp_index.index())),
            timer: driver.new_dyn_timer(),
            force_exit_sidecar: false,
            signaled_sidecar_exit: false,
            vtls_tlb_locked: VtlsTlbLocked {
                vtl1: VtlArray::new(false),
                vtl2: VtlArray::new(false),
            },
            exit_activities: Default::default(),
        };

        T::init(&mut vp);

        Ok(vp)
    }

    /// Returns true if the interrupt controller has work to do.
    fn handle_wake(&mut self) -> VtlArray<bool, 2> {
        let wake_reasons_raw = self.inner.wake_reasons.swap(0, Ordering::SeqCst);
        let wake_reasons_vtl: [WakeReason; 2] = zerocopy::transmute!(wake_reasons_raw);
        for (vtl, wake_reasons) in [
            (GuestVtl::Vtl1, wake_reasons_vtl[1]),
            (GuestVtl::Vtl0, wake_reasons_vtl[0]),
        ] {
            if wake_reasons.message_queues() {
                let pending_sints = self.inner.message_queues[vtl].pending_sints();
                if pending_sints != 0 {
                    // Set SINT interest.
                    let pending_sints = self.inner.message_queues[vtl].pending_sints();
                    let mut masked_sints = 0;

                    // Determine which of the pending sints are masked.
                    for sint in 0..NUM_SINTS as u8 {
                        if pending_sints & (1 << sint) == 0 {
                            continue;
                        }
                        let sint_msr = if let Some(hv) = self.backing.hv(vtl).as_ref() {
                            hv.synic.sint(sint)
                        } else {
                            #[cfg(guest_arch = "x86_64")]
                            let sint_reg =
                                HvX64RegisterName(HvX64RegisterName::Sint0.0 + sint as u32);
                            #[cfg(guest_arch = "aarch64")]
                            let sint_reg =
                                HvArm64RegisterName(HvArm64RegisterName::Sint0.0 + sint as u32);
                            self.runner.get_vp_register(vtl, sint_reg).unwrap().as_u64()
                        };
                        masked_sints |= (HvSynicSint::from(sint_msr).masked() as u16) << sint;
                    }

                    // Drain the queues for all masked SINTs.
                    self.inner.message_queues[vtl].post_pending_messages(masked_sints, |_, _| {
                        Err(HvError::InvalidSynicState)
                    });

                    self.request_sint_notifications(vtl, pending_sints & !masked_sints);
                }
            }

            if wake_reasons.extint() {
                T::request_extint_readiness(self);
            }

            #[cfg(guest_arch = "x86_64")]
            if wake_reasons.hv_start_enable_vtl_vp() {
                T::handle_vp_start_enable_vtl_wake(self, vtl);
            }

            #[cfg(guest_arch = "x86_64")]
            if wake_reasons.update_proxy_irr_filter() {
                // update `proxy_irr_blocked` filter
                debug_assert!(self.partition.isolation.is_hardware_isolated());
                self.update_proxy_irr_filter(vtl);
            }
        }

        wake_reasons_vtl.map(|w| w.intcon()).into()
    }

    fn request_sint_notifications(&mut self, vtl: GuestVtl, sints: u16) {
        if sints == 0 {
            return;
        }

        // Send the SINT notifications to the local synic for non-proxied SINTs.
        let untrusted_sints = if let Some(hv) = self.backing.hv_mut(vtl).as_mut() {
            let proxied_sints = hv.synic.proxied_sints();
            hv.synic.request_sint_readiness(sints & !proxied_sints);
            proxied_sints
        } else {
            !0
        };

        if sints & untrusted_sints != 0 {
            assert_eq!(vtl, GuestVtl::Vtl0);
            T::request_untrusted_sint_readiness(self, sints & untrusted_sints);
        }
    }

    fn vp_index(&self) -> VpIndex {
        self.inner.vp_index()
    }

    #[cfg(guest_arch = "x86_64")]
    fn write_crash_msr(&mut self, msr: u32, value: u64, vtl: GuestVtl) -> Result<(), MsrError> {
        match msr {
            hvdef::HV_X64_MSR_GUEST_CRASH_CTL => {
                let crash = VtlCrash {
                    vp_index: self.vp_index(),
                    last_vtl: vtl,
                    control: hvdef::GuestCrashCtl::from(value),
                    parameters: self.crash_reg,
                };
                tracelimit::warn_ratelimited!(
                    CVM_ALLOWED,
                    ?crash,
                    "Guest has reported system crash"
                );

                if crash.control.crash_message() {
                    let message_gpa = crash.parameters[3];
                    let message_size = std::cmp::min(crash.parameters[4], hvdef::HV_PAGE_SIZE);
                    let mut message = vec![0; message_size as usize];
                    match self.partition.gm[vtl].read_at(message_gpa, &mut message) {
                        Ok(()) => {
                            let message = String::from_utf8_lossy(&message).into_owned();
                            tracelimit::warn_ratelimited!(
                                CVM_CONFIDENTIAL,
                                message,
                                "Guest has reported a system crash message"
                            );
                        }
                        Err(e) => {
                            tracelimit::warn_ratelimited!(
                                CVM_ALLOWED,
                                ?e,
                                "Failed to read crash message"
                            );
                        }
                    }
                }

                self.partition.crash_notification_send.send(crash);
            }
            hvdef::HV_X64_MSR_GUEST_CRASH_P0
            | hvdef::HV_X64_MSR_GUEST_CRASH_P1
            | hvdef::HV_X64_MSR_GUEST_CRASH_P2
            | hvdef::HV_X64_MSR_GUEST_CRASH_P3
            | hvdef::HV_X64_MSR_GUEST_CRASH_P4 => {
                self.crash_reg[(msr - hvdef::HV_X64_MSR_GUEST_CRASH_P0) as usize] = value;
            }
            _ => return Err(MsrError::Unknown),
        }
        Ok(())
    }

    #[cfg(guest_arch = "x86_64")]
    fn read_crash_msr(&self, msr: u32, _vtl: GuestVtl) -> Result<u64, MsrError> {
        let v = match msr {
            // Reads of CRASH_CTL report our supported capabilities, not the
            // current value.
            hvdef::HV_X64_MSR_GUEST_CRASH_CTL => hvdef::GuestCrashCtl::new()
                .with_crash_notify(true)
                .with_crash_message(true)
                .into(),
            hvdef::HV_X64_MSR_GUEST_CRASH_P0 => self.crash_reg[0],
            hvdef::HV_X64_MSR_GUEST_CRASH_P1 => self.crash_reg[1],
            hvdef::HV_X64_MSR_GUEST_CRASH_P2 => self.crash_reg[2],
            hvdef::HV_X64_MSR_GUEST_CRASH_P3 => self.crash_reg[3],
            hvdef::HV_X64_MSR_GUEST_CRASH_P4 => self.crash_reg[4],
            _ => return Err(MsrError::Unknown),
        };
        Ok(v)
    }

    /// Emulates an instruction due to a memory access exit.
    #[cfg(guest_arch = "x86_64")]
    async fn emulate<D: CpuIo>(
        &mut self,
        devices: &D,
        interruption_pending: bool,
        vtl: GuestVtl,
        cache: T::EmulationCache,
    ) -> Result<(), VpHaltReason>
    where
        for<'b> UhEmulationState<'b, 'a, D, T>: virt_support_x86emu::emulate::EmulatorSupport,
    {
        let guest_memory = &self.partition.gm[vtl];
        let (kx_guest_memory, ux_guest_memory) = match vtl {
            GuestVtl::Vtl0 => (
                &self.partition.vtl0_kernel_exec_gm,
                &self.partition.vtl0_user_exec_gm,
            ),
            GuestVtl::Vtl1 => (guest_memory, guest_memory),
        };
        let emu_mem = virt_support_x86emu::emulate::EmulatorMemoryAccess {
            gm: guest_memory,
            kx_gm: kx_guest_memory,
            ux_gm: ux_guest_memory,
        };
        let mut emulation_state = UhEmulationState {
            vp: &mut *self,
            interruption_pending,
            devices,
            vtl,
            cache,
        };

        virt_support_x86emu::emulate::emulate(&mut emulation_state, &emu_mem, devices).await
    }

    /// Emulates an instruction due to a memory access exit.
    #[cfg(guest_arch = "aarch64")]
    async fn emulate<D: CpuIo>(
        &mut self,
        devices: &D,
        intercept_state: &aarch64emu::InterceptState,
        vtl: GuestVtl,
        cache: T::EmulationCache,
    ) -> Result<(), VpHaltReason>
    where
        for<'b> UhEmulationState<'b, 'a, D, T>: virt_support_aarch64emu::emulate::EmulatorSupport,
    {
        let guest_memory = &self.partition.gm[vtl];
        virt_support_aarch64emu::emulate::emulate(
            &mut UhEmulationState {
                vp: &mut *self,
                interruption_pending: intercept_state.interruption_pending,
                devices,
                vtl,
                cache,
            },
            intercept_state,
            guest_memory,
            devices,
        )
        .await
    }

    #[cfg(guest_arch = "x86_64")]
    fn update_proxy_irr_filter(&mut self, vtl: GuestVtl) {
        assert_eq!(vtl, GuestVtl::Vtl0);
        let mut irr_bits: BitArray<[u32; 8], Lsb0> = BitArray::new(Default::default());

        // Get all not masked && proxy SINT vectors
        if let Some(hv) = self.backing.hv(vtl).as_ref() {
            for sint in 0..NUM_SINTS as u8 {
                let sint_msr = hv.synic.sint(sint);
                let hv_sint = HvSynicSint::from(sint_msr);
                // When vmbus relay is active, then SINT will not be proxied
                // in case of non-relay, guest will setup proxied sint
                if (hv_sint.proxy() || self.partition.vmbus_relay) && !hv_sint.masked() {
                    irr_bits.set(hv_sint.vector() as usize, true);
                }
            }
        }

        // Get all device vectors
        self.partition.fill_device_vectors(vtl, &mut irr_bits);

        // Update `proxy_irr_blocked` filter in run page
        self.runner
            .update_proxy_irr_filter_vtl0(&irr_bits.into_inner());
    }
}

fn signal_mnf(dev: &impl CpuIo, connection_id: u32) {
    if let Err(err) = dev.signal_synic_event(Vtl::Vtl0, connection_id, 0) {
        tracelimit::warn_ratelimited!(
            CVM_ALLOWED,
            error = &err as &dyn std::error::Error,
            connection_id,
            "failed to signal mnf"
        );
    }
}

/// Yields execution back to the executor.
async fn yield_now() {
    let mut yielded = false;
    poll_fn(|cx| {
        if !yielded {
            // Wake the waker so that this task gets to run again.
            cx.waker().wake_by_ref();
            yielded = true;
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    })
    .await;
}

struct UhEmulationState<'a, 'b, T: CpuIo, U: Backing> {
    vp: &'a mut UhProcessor<'b, U>,
    interruption_pending: bool,
    devices: &'a T,
    vtl: GuestVtl,
    cache: U::EmulationCache,
}

impl<T: CpuIo, U: Backing> EmulatorMonitorSupport for UhEmulationState<'_, '_, T, U> {
    fn check_write(&self, gpa: u64, bytes: &[u8]) -> bool {
        self.vp
            .partition
            .monitor_page
            .check_write(gpa, bytes, |connection_id| {
                signal_mnf(self.devices, connection_id);
            })
    }

    fn check_read(&self, gpa: u64, bytes: &mut [u8]) -> bool {
        self.vp.partition.monitor_page.check_read(gpa, bytes)
    }
}

struct UhHypercallHandler<'a, 'b, T, B: Backing> {
    vp: &'a mut UhProcessor<'b, B>,
    bus: &'a T,
    /// Indicates if the handler is for trusted hypercalls in case hardware isolation is in use. A
    /// hypercall is trusted if it was made by the guest using a regular vmcall instruction, without
    /// using any host-visible mechanisms. An untrusted hypercall was intercepted from the
    /// hypervisor, such as one made by the guest using an isolated mechanism such as tdcall or
    /// GHCB.
    ///
    /// This should always be false if hardware isolation is not in use, as the distinction does
    /// not exist in that case.
    trusted: bool,
    intercepted_vtl: GuestVtl,
}

impl<T, B: Backing> UhHypercallHandler<'_, '_, T, B> {
    fn target_vtl_no_higher(&self, target_vtl: Vtl) -> Result<GuestVtl, HvError> {
        if Vtl::from(self.intercepted_vtl) < target_vtl {
            return Err(HvError::AccessDenied);
        }
        Ok(target_vtl.try_into().unwrap())
    }
}

impl<T, B: Backing> hv1_hypercall::GetVpIndexFromApicId for UhHypercallHandler<'_, '_, T, B> {
    fn get_vp_index_from_apic_id(
        &mut self,
        partition_id: u64,
        target_vtl: Vtl,
        apic_ids: &[u32],
        vp_indices: &mut [u32],
    ) -> HvRepResult {
        tracing::debug!(partition_id, ?target_vtl, "HvGetVpIndexFromApicId");

        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err((HvError::InvalidPartitionId, 0));
        }

        let _target_vtl = self.target_vtl_no_higher(target_vtl).map_err(|e| (e, 0))?;

        #[cfg(guest_arch = "aarch64")]
        if true {
            let _ = apic_ids;
            let _ = vp_indices;
            todo!("AARCH64_TODO");
        }

        #[cfg(guest_arch = "x86_64")]
        for (i, (&apic_id, vp_index)) in apic_ids.iter().zip(vp_indices).enumerate() {
            *vp_index = self
                .vp
                .partition
                .vps
                .iter()
                .find(|vp| vp.vp_info.apic_id == apic_id)
                .ok_or((HvError::InvalidParameter, i))?
                .vp_info
                .base
                .vp_index
                .index()
        }

        Ok(())
    }
}

#[cfg(guest_arch = "aarch64")]
impl<T: CpuIo, B: Backing> Arm64RegisterState for UhHypercallHandler<'_, '_, T, B> {
    fn pc(&mut self) -> u64 {
        self.vp
            .runner
            .get_vp_register(self.intercepted_vtl, HvArm64RegisterName::XPc)
            .expect("get vp register cannot fail")
            .as_u64()
    }

    fn set_pc(&mut self, pc: u64) {
        self.vp
            .runner
            .set_vp_register(self.intercepted_vtl, HvArm64RegisterName::XPc, pc.into())
            .expect("set vp register cannot fail");
    }

    fn x(&mut self, n: u8) -> u64 {
        self.vp
            .runner
            .get_vp_register(
                self.intercepted_vtl,
                HvArm64RegisterName(HvArm64RegisterName::X0.0 + n as u32),
            )
            .expect("get vp register cannot fail")
            .as_u64()
    }

    fn set_x(&mut self, n: u8, v: u64) {
        self.vp
            .runner
            .set_vp_register(
                self.intercepted_vtl,
                HvArm64RegisterName(HvArm64RegisterName::X0.0 + n as u32),
                v.into(),
            )
            .expect("set vp register cannot fail")
    }
}

impl<T: CpuIo, B: Backing> hv1_hypercall::PostMessage for UhHypercallHandler<'_, '_, T, B> {
    fn post_message(&mut self, connection_id: u32, message: &[u8]) -> hvdef::HvResult<()> {
        tracing::trace!(
            connection_id,
            self.trusted,
            "handling post message intercept"
        );

        self.bus.post_synic_message(
            self.intercepted_vtl.into(),
            connection_id,
            self.trusted,
            message,
        )
    }
}

impl<T: CpuIo, B: Backing> hv1_hypercall::SignalEvent for UhHypercallHandler<'_, '_, T, B> {
    fn signal_event(&mut self, connection_id: u32, flag: u16) -> hvdef::HvResult<()> {
        tracing::trace!(connection_id, "handling signal event intercept");

        self.bus
            .signal_synic_event(self.intercepted_vtl.into(), connection_id, flag)
    }
}

impl<T: CpuIo, B: Backing> UhHypercallHandler<'_, '_, T, B> {
    fn retarget_virtual_interrupt(
        &mut self,
        device_id: u64,
        address: u64,
        data: u32,
        vector: u32,
        multicast: bool,
        target_processors: ProcessorSet<'_>,
    ) -> hvdef::HvResult<()> {
        let target_processors = Vec::from_iter(target_processors);
        let vpci_params = vmcore::vpci_msi::VpciInterruptParameters {
            vector,
            multicast,
            target_processors: &target_processors,
        };

        self.vp
            .partition
            .software_devices
            .as_ref()
            .expect("should exist if this intercept is registered or this is a CVM")
            .retarget_interrupt(device_id, address, data, &vpci_params)
    }
}

impl<T, B: Backing> hv1_hypercall::ExtendedQueryCapabilities for UhHypercallHandler<'_, '_, T, B> {
    fn query_extended_capabilities(&mut self) -> hvdef::HvResult<u64> {
        // This capability is not actually supported. However Windows may unconditionally issue this
        // hypercall. Return InvalidHypercallCode as the error status. This is the same as not
        // implementing this at all, but has the advantage of not causing generating error messages.
        Err(HvError::InvalidHypercallCode)
    }
}
