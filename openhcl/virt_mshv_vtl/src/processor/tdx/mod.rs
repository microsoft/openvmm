// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Processor support for TDX partitions.

mod tlb_flush;

use super::BackingPrivate;
use super::BackingSharedParams;
use super::HardwareIsolatedBacking;
use super::UhEmulationState;
use super::UhHypercallHandler;
use super::hardware_cvm;
use super::vp_state;
use super::vp_state::UhVpStateAccess;
use crate::BackingShared;
use crate::GuestVtl;
use crate::IsolationType;
use crate::TlbFlushLockAccess;
use crate::UhCvmPartitionState;
use crate::UhCvmVpState;
use crate::UhPartitionInner;
use crate::UhPartitionNewParams;
use crate::UhProcessor;
use crate::WakeReason;
use crate::get_tsc_frequency;
use cvm_tracing::CVM_ALLOWED;
use cvm_tracing::CVM_CONFIDENTIAL;
use guestmem::GuestMemory;
use hcl::ioctl::ProcessorRunner;
use hcl::ioctl::tdx::Tdx;
use hcl::ioctl::tdx::TdxPrivateRegs;
use hcl::protocol::hcl_intr_offload_flags;
use hcl::protocol::tdx_tdg_vp_enter_exit_info;
use hv1_emulator::hv::ProcessorVtlHv;
use hv1_emulator::synic::GlobalSynic;
use hv1_emulator::synic::ProcessorSynic;
use hv1_hypercall::AsHandler;
use hv1_hypercall::HvRepResult;
use hv1_hypercall::HypercallIo;
use hv1_structs::ProcessorSet;
use hv1_structs::VtlArray;
use hvdef::HV_PAGE_SIZE;
use hvdef::HvError;
use hvdef::HvSynicSimpSiefp;
use hvdef::HvX64PendingExceptionEvent;
use hvdef::HvX64RegisterName;
use hvdef::Vtl;
use hvdef::hypercall::HvFlushFlags;
use hvdef::hypercall::HvGvaRange;
use inspect::Inspect;
use inspect::InspectMut;
use inspect_counters::Counter;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use thiserror::Error;
use tlb_flush::FLUSH_GVA_LIST_SIZE;
use tlb_flush::TdxFlushState;
use tlb_flush::TdxPartitionFlushState;
use virt::EmulatorMonitorSupport;
use virt::Processor;
use virt::VpHaltReason;
use virt::VpIndex;
use virt::io::CpuIo;
use virt::state::StateElement;
use virt::vp;
use virt::vp::AccessVpState;
use virt::vp::MpState;
use virt::vp::Registers;
use virt::x86::MsrError;
use virt::x86::MsrErrorExt;
use virt::x86::SegmentRegister;
use virt::x86::TableRegister;
use virt_support_apic::ApicClient;
use virt_support_apic::OffloadNotSupported;
use virt_support_x86emu::emulate::EmulatedMemoryOperation;
use virt_support_x86emu::emulate::EmulatorSupport as X86EmulatorSupport;
use virt_support_x86emu::emulate::TranslateMode;
use virt_support_x86emu::emulate::emulate_insn_memory_op;
use virt_support_x86emu::emulate::emulate_io;
use virt_support_x86emu::emulate::emulate_translate_gva;
use virt_support_x86emu::translate::TranslationRegisters;
use vmcore::vmtime::VmTimeAccess;
use x86defs::RFlags;
use x86defs::X64_CR0_ET;
use x86defs::X64_CR0_NE;
use x86defs::X64_CR0_PE;
use x86defs::X64_CR0_PG;
use x86defs::X64_CR4_MCE;
use x86defs::X64_CR4_UMIP;
use x86defs::X64_CR4_VMXE;
use x86defs::X64_EFER_FFXSR;
use x86defs::X64_EFER_LMA;
use x86defs::X64_EFER_LME;
use x86defs::X64_EFER_NXE;
use x86defs::X64_EFER_SVME;
use x86defs::X86X_MSR_EFER;
use x86defs::apic::X2APIC_MSR_BASE;
use x86defs::tdx::TdCallResultCode;
use x86defs::tdx::TdVmCallR10Result;
use x86defs::tdx::TdxGp;
use x86defs::tdx::TdxInstructionInfo;
use x86defs::tdx::TdxL2Ctls;
use x86defs::tdx::TdxVpEnterRaxResult;
use x86defs::vmx::ApicPage;
use x86defs::vmx::ApicRegister;
use x86defs::vmx::CR_ACCESS_TYPE_LMSW;
use x86defs::vmx::CR_ACCESS_TYPE_MOV_TO_CR;
use x86defs::vmx::CrAccessQualification;
use x86defs::vmx::ExitQualificationIo;
use x86defs::vmx::GdtrOrIdtrInstruction;
use x86defs::vmx::GdtrOrIdtrInstructionInfo;
use x86defs::vmx::INTERRUPT_TYPE_EXTERNAL;
use x86defs::vmx::INTERRUPT_TYPE_HARDWARE_EXCEPTION;
use x86defs::vmx::INTERRUPT_TYPE_NMI;
use x86defs::vmx::IO_SIZE_8_BIT;
use x86defs::vmx::IO_SIZE_16_BIT;
use x86defs::vmx::IO_SIZE_32_BIT;
use x86defs::vmx::Interruptibility;
use x86defs::vmx::InterruptionInformation;
use x86defs::vmx::LdtrOrTrInstruction;
use x86defs::vmx::LdtrOrTrInstructionInfo;
use x86defs::vmx::ProcessorControls;
use x86defs::vmx::SecondaryProcessorControls;
use x86defs::vmx::VMX_ENTRY_CONTROL_LONG_MODE_GUEST;
use x86defs::vmx::VMX_FEATURE_CONTROL_LOCKED;
use x86defs::vmx::VmcsField;
use x86defs::vmx::VmxEptExitQualification;
use x86defs::vmx::VmxExit;
use x86defs::vmx::VmxExitBasic;
use x86emu::Gp;
use x86emu::Segment;

/// MSRs that are allowed to be read by the guest without interception.
const MSR_ALLOWED_READ: &[u32] = &[
    x86defs::X86X_MSR_TSC,
    x86defs::X86X_MSR_TSC_AUX,
    X86X_MSR_EFER,
    x86defs::X86X_MSR_STAR,
    x86defs::X86X_MSR_LSTAR,
    x86defs::X86X_MSR_SFMASK,
    x86defs::X86X_MSR_SYSENTER_CS,
    x86defs::X86X_MSR_SYSENTER_ESP,
    x86defs::X86X_MSR_SYSENTER_EIP,
];

/// MSRs that are allowed to be read and written by the guest without interception.
const MSR_ALLOWED_READ_WRITE: &[u32] = &[
    x86defs::X64_MSR_FS_BASE,
    x86defs::X64_MSR_GS_BASE,
    x86defs::X64_MSR_KERNEL_GS_BASE,
    x86defs::X86X_MSR_SPEC_CTRL,
    x86defs::X86X_MSR_U_CET,
    x86defs::X86X_MSR_S_CET,
    x86defs::X86X_MSR_PL0_SSP,
    x86defs::X86X_MSR_PL1_SSP,
    x86defs::X86X_MSR_PL2_SSP,
    x86defs::X86X_MSR_PL3_SSP,
    x86defs::X86X_MSR_INTERRUPT_SSP_TABLE_ADDR,
    x86defs::X86X_IA32_MSR_XFD,
    x86defs::X86X_IA32_MSR_XFD_ERR,
];

#[derive(Debug, Error)]
#[error("unknown exit {0:#x?}")]
struct UnknownVmxExit(VmxExit);

#[derive(Debug, Error)]
#[error("bad guest state on VP.ENTER")]
struct VmxBadGuestState;

#[derive(Debug, Error)]
#[error("failed to run")]
struct TdxRunVpError(#[source] hcl::ioctl::Error);

#[derive(Debug)]
struct TdxExit<'a>(&'a tdx_tdg_vp_enter_exit_info);

impl TdxExit<'_> {
    fn code(&self) -> TdxVpEnterRaxResult {
        self.0.rax.into()
    }
    fn qualification(&self) -> u64 {
        self.0.rcx
    }
    fn gla(&self) -> Option<u64> {
        // Only valid for EPT exits.
        if self.code().vmx_exit().basic_reason() == VmxExitBasic::EPT_VIOLATION {
            Some(self.0.rdx)
        } else {
            None
        }
    }
    fn gpa(&self) -> Option<u64> {
        // Only valid for EPT exits.
        if self.code().vmx_exit().basic_reason() == VmxExitBasic::EPT_VIOLATION {
            Some(self.0.r8)
        } else {
            None
        }
    }
    fn _exit_interruption_info(&self) -> InterruptionInformation {
        (self.0.r9 as u32).into()
    }
    fn _exit_interruption_error_code(&self) -> u32 {
        (self.0.r9 >> 32) as u32
    }
    fn idt_vectoring_info(&self) -> InterruptionInformation {
        (self.0.r10 as u32).into()
    }
    fn idt_vectoring_error_code(&self) -> u32 {
        (self.0.r10 >> 32) as u32
    }
    fn instr_info(&self) -> TdxInstructionInfo {
        self.0.r11.into()
    }
    fn cs(&self) -> SegmentRegister {
        SegmentRegister {
            selector: self.0.rsi as u16,
            base: self.0.rdi,
            limit: (self.0.rsi >> 32) as u32,
            attributes: (self.0.rsi >> 16) as u16,
        }
    }
    fn cpl(&self) -> u8 {
        self.0.r12 as u8 & 3
    }
}

/// Registers that can be virtual and shadowed.
#[derive(Debug, Inspect)]
enum ShadowedRegister {
    Cr0,
    Cr4,
}

impl ShadowedRegister {
    fn name(&self) -> &'static str {
        match self {
            Self::Cr0 => "cr0",
            Self::Cr4 => "cr4",
        }
    }

    fn physical_vmcs_field(&self) -> VmcsField {
        match self {
            Self::Cr0 => VmcsField::VMX_VMCS_GUEST_CR0,
            Self::Cr4 => VmcsField::VMX_VMCS_GUEST_CR4,
        }
    }

    fn shadow_vmcs_field(&self) -> VmcsField {
        match self {
            Self::Cr0 => VmcsField::VMX_VMCS_CR0_READ_SHADOW,
            Self::Cr4 => VmcsField::VMX_VMCS_CR4_READ_SHADOW,
        }
    }

    fn guest_owned_mask(&self) -> u64 {
        // Control register bits that are guest owned by default. A bit is guest
        // owned when the physical register bit is always set to the virtual
        // register bit (subject to validation of the virtual register).
        match self {
            Self::Cr0 => {
                X64_CR0_ET
                    | x86defs::X64_CR0_MP
                    | x86defs::X64_CR0_EM
                    | x86defs::X64_CR0_TS
                    | x86defs::X64_CR0_WP
                    | x86defs::X64_CR0_AM
                    | X64_CR0_PE
                    | X64_CR0_PG
            }
            Self::Cr4 => {
                x86defs::X64_CR4_VME
                    | x86defs::X64_CR4_PVI
                    | x86defs::X64_CR4_TSD
                    | x86defs::X64_CR4_DE
                    | x86defs::X64_CR4_PSE
                    | x86defs::X64_CR4_PAE
                    | x86defs::X64_CR4_PGE
                    | x86defs::X64_CR4_PCE
                    | x86defs::X64_CR4_FXSR
                    | x86defs::X64_CR4_XMMEXCPT
                    | X64_CR4_UMIP
                    | x86defs::X64_CR4_LA57
                    | x86defs::X64_CR4_RWFSGS
                    | x86defs::X64_CR4_PCIDE
                    | x86defs::X64_CR4_OSXSAVE
                    | x86defs::X64_CR4_SMEP
                    | x86defs::X64_CR4_SMAP
                    | x86defs::X64_CR4_CET
            }
        }
    }
}

/// A virtual register that is shadowed by the virtstack.
///
/// Some bits are owned by the guest while others are owned by the virtstack,
/// due to TDX requirements.
#[derive(Inspect)]
struct VirtualRegister {
    /// The register being shadowed.
    register: ShadowedRegister,
    /// The VTL this register is shadowed for.
    vtl: GuestVtl,
    /// The value the guest sees.
    shadow_value: u64,
    /// Additional constraints on bits.
    allowed_bits: u64,
}

impl VirtualRegister {
    fn new(reg: ShadowedRegister, vtl: GuestVtl, initial_value: u64, allowed_bits: u64) -> Self {
        Self {
            register: reg,
            vtl,
            shadow_value: initial_value,
            allowed_bits,
        }
    }

    /// Write a new value to the virtual register. This updates host owned bits
    /// in the shadowed value, and updates guest owned bits in the physical
    /// register in the vmcs.
    fn write<'a>(
        &mut self,
        value: u64,
        runner: &mut ProcessorRunner<'a, Tdx<'a>>,
    ) -> Result<(), vp_state::Error> {
        tracing::trace!(?self.register, value, "write virtual register");

        if value & !self.allowed_bits != 0 {
            return Err(vp_state::Error::InvalidValue(
                value,
                self.register.name(),
                "disallowed bit set",
            ));
        }

        // If guest owned bits of the physical register have changed, then update
        // the guest owned bits of the physical field.
        let old_physical_reg = runner.read_vmcs64(self.vtl, self.register.physical_vmcs_field());

        tracing::trace!(old_physical_reg, "old_physical_reg");

        let guest_owned_mask = self.register.guest_owned_mask();
        if (old_physical_reg ^ value) & guest_owned_mask != 0 {
            let new_physical_reg =
                (old_physical_reg & !guest_owned_mask) | (value & guest_owned_mask);

            tracing::trace!(new_physical_reg, "new_physical_reg");

            runner.write_vmcs64(
                self.vtl,
                self.register.physical_vmcs_field(),
                !0,
                new_physical_reg,
            );
        }

        self.shadow_value = value;
        runner.write_vmcs64(self.vtl, self.register.shadow_vmcs_field(), !0, value);
        Ok(())
    }

    fn read<'a>(&self, runner: &ProcessorRunner<'a, Tdx<'a>>) -> u64 {
        let physical_reg = runner.read_vmcs64(self.vtl, self.register.physical_vmcs_field());

        // Get the bits owned by the host from the shadow and the bits owned by the
        // guest from the physical value.
        let guest_owned_mask = self.register.guest_owned_mask();
        (self.shadow_value & !self.register.guest_owned_mask()) | (physical_reg & guest_owned_mask)
    }
}

/// Interface for managing lower VTL timer deadlines via TDX L2-VM TSC Deadline
/// Timer capability.
///
/// This allows VTL2 to set an execution deadline for lower VTLs, in absolute
/// virtual TSC units. If the lower VTL is running when the deadline time
/// arrives, it exits to VTL2 with exit reason `VmxExitBasic::TIMER_EXPIRED`.
/// If the TSC deadline is in the past during entry into lower VTL (i.e., TSC
/// deadline value is lower than the current virtual TSC value), it will immediately
/// exit back to VTL2 with exit reason `VmxExitBasic::TIMER_EXPIRED`.
///
/// The TSC deadline is set using `TDG.VP.WR` for `TDVPS.TSC_DEADLINE[L2-VM Index]`.
/// The actual `TDG.VP.WR` call to set the deadline is made by the `mshv_vtl` driver
///  before entering the lower VTL.
struct TdxTscDeadlineService {
    // Fixed-point scale factor to convert 100ns to TSC units.
    tsc_scale_100ns: u128,
}

impl TdxTscDeadlineService {
    /// Convert hypervisor reference time (in 100ns) to TSC units.
    fn ref_time_to_tsc(&self, ref_time: u64) -> u64 {
        // Use fixed-point multiplication to calculate:
        // tsc_ticks = (time_100ns /  10_000_000) * tsc_frequency
        ((ref_time as u128 * self.tsc_scale_100ns) >> 64) as u64
    }

    /// Returns true if `ref_time` is before `ref_time_last`.
    ///
    /// Note that this uses wrapping arithmetic to handle 64-bit timestamp wraparound
    ///  and hence this is not transitive: if `a` is before `b`, and `b` is before `c`,
    /// `a` may still appear after `c` if they are too far apart in the circular space.
    fn is_before(ref_time: u64, ref_time_last: u64) -> bool {
        let delta = ref_time.wrapping_sub(ref_time_last);
        (delta as i64) < 0
    }
}

impl hardware_cvm::HardwareIsolatedGuestTimer<TdxBacked> for TdxTscDeadlineService {
    fn is_hardware_virtualized(&self) -> bool {
        true
    }

    /// Update the virtual timer deadline in the processor's context shared with kernel.
    /// This deadline will be set by `mshv_vtl` using
    /// `TDG.VP.WR(TDVPS.TSC_DEADLINE[L2-VM Index])` before entering into lower VTL.
    fn update_deadline(
        &self,
        vp: &mut UhProcessor<'_, TdxBacked>,
        ref_time_now: u64,
        ref_time_next: u64,
    ) {
        let vp_state = vp
            .backing
            .tsc_deadline_state
            .as_mut()
            .expect("TdxTscDeadlineService requires tsc_deadline_state");

        // Update needed only if no deadline is set or the new time is earlier.
        if vp_state
            .deadline_100ns
            .is_none_or(|last| Self::is_before(ref_time_next, last))
        {
            // Record the new reference time.
            vp_state.deadline_100ns = Some(ref_time_next);

            let state = vp.runner.tdx_l2_tsc_deadline_state_mut();
            if vp_state
                .last_deadline_100ns
                .is_none_or(|last| last != ref_time_next)
            {
                let ref_time_from_now = ref_time_next.saturating_sub(ref_time_now);
                let tsc_delta = self.ref_time_to_tsc(ref_time_from_now);
                let deadline = safe_intrinsics::rdtsc().wrapping_add(tsc_delta);

                state.deadline = deadline;
                state.update_deadline = 1;

                tracing::trace!(
                    ref_time_from_now,
                    tsc_delta,
                    deadline,
                    "updating deadline for TDX L2-VM TSC deadline timer"
                );
            } else {
                state.update_deadline = 0;
            }
        }
    }

    /// Clears the virtual timer deadline in the processor context.
    fn clear_deadline(&self, vp: &mut UhProcessor<'_, TdxBacked>) {
        let vp_state = vp
            .backing
            .tsc_deadline_state
            .as_mut()
            .expect("TdxTscDeadlineService requires tsc_deadline_state");

        vp_state.deadline_100ns = None;

        let state = vp.runner.tdx_l2_tsc_deadline_state_mut();
        state.update_deadline = 0;
    }

    /// Synchronize armed deadline state in the processor context.
    fn sync_deadline_state(&self, vp: &mut UhProcessor<'_, TdxBacked>) {
        let vp_state = vp
            .backing
            .tsc_deadline_state
            .as_mut()
            .expect("TdxTscDeadlineService requires tsc_deadline_state");

        vp_state.last_deadline_100ns = vp_state.deadline_100ns;
    }
}

/// Per-VP state for TDX L2-VM TSC deadline timer.
#[derive(Inspect, Default)]
struct TdxTscDeadline {
    /// Next deadline to be armed (in 100ns units).
    #[inspect(hex)]
    deadline_100ns: Option<u64>,
    /// Deadline (in 100ns units) armed by `mshv_vtl` driver during previous entry
    /// into lower VTL.
    #[inspect(hex)]
    last_deadline_100ns: Option<u64>,
}

/// Backing for TDX partitions.
#[derive(InspectMut)]
pub struct TdxBacked {
    #[inspect(mut)]
    vtls: VtlArray<TdxVtl, 2>,

    untrusted_synic: Option<ProcessorSynic>,
    #[inspect(hex, iter_by_index)]
    eoi_exit_bitmap: [u64; 4],

    /// A mapped page used for issuing INVGLA hypercalls.
    #[inspect(skip)]
    flush_page: user_driver::memory::MemoryBlock,

    #[inspect(flatten)]
    cvm: UhCvmVpState,

    /// Per-processor state for [`TdxTscDeadlineService`].
    #[inspect(flatten)]
    tsc_deadline_state: Option<TdxTscDeadline>,
}

#[derive(InspectMut)]
struct TdxVtl {
    /// The EFER value for this VP.
    efer: u64,
    /// Virtual cr0.
    cr0: VirtualRegister,
    /// Virtual cr4.
    cr4: VirtualRegister,

    // CSTAR doesn't exist on TDX, but Windows likes to verify that values are sticky.
    msr_cstar: u64,

    tpr_threshold: u8,
    #[inspect(skip)]
    processor_controls: ProcessorControls,
    #[inspect(skip)]
    interruption_information: InterruptionInformation,
    exception_error_code: u32,
    interruption_set: bool,

    #[inspect(mut)]
    private_regs: TdxPrivateRegs,

    /// TDX only TLB flush state.
    flush_state: TdxFlushState,

    enter_stats: EnterStats,
    exit_stats: ExitStats,
}

#[derive(Default)]
pub struct TdxEmulationCache {
    segs: [Option<SegmentRegister>; 6],
    cr0: Option<u64>,
}

#[derive(Inspect, Default)]
struct EnterStats {
    success: Counter,
    host_routed_async: Counter,
    l2_exit_pending_intr: Counter,
    pending_intr: Counter,
    host_routed_td_vmcall: Counter,
}

#[derive(Inspect, Default)]
struct ExitStats {
    io: Counter,
    msr_read: Counter,
    msr_write: Counter,
    ept_violation: Counter,
    cpuid: Counter,
    cr_access: Counter,
    xsetbv: Counter,
    tpr_below_threshold: Counter,
    interrupt_window: Counter,
    nmi_window: Counter,
    vmcall: Counter,
    smi_intr: Counter,
    wbinvd: Counter,
    hw_interrupt: Counter,
    tdcall: Counter,
    hlt: Counter,
    pause: Counter,
    needs_interrupt_reinject: Counter,
    exception: Counter,
    descriptor_table: Counter,
    timer_expired: Counter,
}

enum UhDirectOverlay {
    Sipp,
    Sifp,
    Count,
}

impl HardwareIsolatedBacking for TdxBacked {
    fn cvm_state(&self) -> &UhCvmVpState {
        &self.cvm
    }

    fn cvm_state_mut(&mut self) -> &mut UhCvmVpState {
        &mut self.cvm
    }

    fn cvm_partition_state(shared: &Self::Shared) -> &UhCvmPartitionState {
        &shared.cvm
    }

    fn switch_vtl(this: &mut UhProcessor<'_, Self>, _source_vtl: GuestVtl, target_vtl: GuestVtl) {
        // The GPs, Fxsave, and CR2 are saved in the shared kernel state. No copying needed.
        // Debug registers and XFEM are shared architecturally. No copying needed.

        this.backing.cvm_state_mut().exit_vtl = target_vtl;
    }

    fn translation_registers(
        &self,
        this: &UhProcessor<'_, Self>,
        vtl: GuestVtl,
    ) -> TranslationRegisters {
        let cr0 = this.backing.vtls[vtl].cr0.read(&this.runner);
        let cr4 = this.backing.vtls[vtl].cr4.read(&this.runner);
        let efer = this.backing.vtls[vtl].efer;
        let cr3 = this.runner.read_vmcs64(vtl, VmcsField::VMX_VMCS_GUEST_CR3);
        let ss = this.read_segment(vtl, TdxSegmentReg::Ss).into();
        let rflags = this.backing.vtls[vtl].private_regs.rflags;

        TranslationRegisters {
            cr0,
            cr4,
            efer,
            cr3,
            ss,
            rflags,
            encryption_mode: this.partition.caps.vtom.map_or(
                virt_support_x86emu::translate::EncryptionMode::None,
                virt_support_x86emu::translate::EncryptionMode::Vtom,
            ),
        }
    }

    fn tlb_flush_lock_access<'a>(
        vp_index: Option<VpIndex>,
        partition: &'a UhPartitionInner,
        shared: &'a Self::Shared,
    ) -> impl TlbFlushLockAccess + 'a {
        TdxTlbLockFlushAccess {
            vp_index,
            partition,
            shared,
        }
    }

    fn pending_event_vector(this: &UhProcessor<'_, Self>, vtl: GuestVtl) -> Option<u8> {
        let event_inject = this.backing.vtls[vtl].interruption_information;
        if event_inject.valid() {
            Some(event_inject.vector())
        } else {
            None
        }
    }

    fn set_pending_exception(
        this: &mut UhProcessor<'_, Self>,
        vtl: GuestVtl,
        event: HvX64PendingExceptionEvent,
    ) {
        let new_intr = InterruptionInformation::new()
            .with_valid(true)
            .with_deliver_error_code(event.deliver_error_code())
            .with_vector(event.vector().try_into().unwrap())
            .with_interruption_type(INTERRUPT_TYPE_HARDWARE_EXCEPTION);

        this.backing.vtls[vtl].interruption_information = new_intr;
        this.backing.vtls[vtl].exception_error_code = event.error_code();
    }

    fn cr0(this: &UhProcessor<'_, Self>, vtl: GuestVtl) -> u64 {
        this.read_cr0(vtl)
    }

    fn cr4(this: &UhProcessor<'_, Self>, vtl: GuestVtl) -> u64 {
        this.read_cr4(vtl)
    }

    fn intercept_message_state(
        this: &UhProcessor<'_, Self>,
        vtl: GuestVtl,
        include_optional_state: bool,
    ) -> super::InterceptMessageState {
        let exit = TdxExit(this.runner.tdx_vp_enter_exit_info());
        let backing_vtl = &this.backing.vtls[vtl];
        let shared_gps = this.runner.tdx_enter_guest_gps();

        super::InterceptMessageState {
            instruction_length_and_cr8: exit.instr_info().length() as u8,
            cpl: exit.cpl(),
            efer_lma: backing_vtl.efer & X64_EFER_LMA != 0,
            cs: exit.cs().into(),
            rip: backing_vtl.private_regs.rip,
            rflags: backing_vtl.private_regs.rflags,
            rax: shared_gps[TdxGp::RAX],
            rdx: shared_gps[TdxGp::RDX],
            optional: if include_optional_state {
                Some(super::InterceptMessageOptionalState {
                    ds: this.read_segment(vtl, TdxSegmentReg::Ds).into(),
                    es: this.read_segment(vtl, TdxSegmentReg::Es).into(),
                })
            } else {
                None
            },
            rcx: shared_gps[TdxGp::RCX],
            rsi: shared_gps[TdxGp::RSI],
            rdi: shared_gps[TdxGp::RDI],
        }
    }

    fn cr_intercept_registration(
        this: &mut UhProcessor<'_, Self>,
        intercept_control: hvdef::HvRegisterCrInterceptControl,
    ) {
        // Today we only support intercepting VTL 0 on behalf of VTL 1.
        let vtl = GuestVtl::Vtl0;
        let intercept_masks = &this
            .backing
            .cvm_state()
            .vtl1
            .as_ref()
            .unwrap()
            .reg_intercept;

        // Update CR0 and CR4 intercept masks in the VMCS.
        this.runner.write_vmcs64(
            vtl,
            VmcsField::VMX_VMCS_CR0_GUEST_HOST_MASK,
            !0,
            this.shared.cr_guest_host_mask(ShadowedRegister::Cr0)
                | if intercept_control.cr0_write() {
                    intercept_masks.cr0_mask
                } else {
                    0
                },
        );
        this.runner.write_vmcs64(
            vtl,
            VmcsField::VMX_VMCS_CR4_GUEST_HOST_MASK,
            !0,
            this.shared.cr_guest_host_mask(ShadowedRegister::Cr4)
                | if intercept_control.cr4_write() {
                    intercept_masks.cr4_mask
                } else {
                    0
                },
        );

        // Update descriptor table intercepts.
        let intercept_tables = intercept_control.gdtr_write()
            | intercept_control.idtr_write()
            | intercept_control.ldtr_write()
            | intercept_control.tr_write();
        this.runner.write_vmcs32(
            vtl,
            VmcsField::VMX_VMCS_SECONDARY_PROCESSOR_CONTROLS,
            SecondaryProcessorControls::new()
                .with_descriptor_table_exiting(true)
                .into_bits(),
            SecondaryProcessorControls::new()
                .with_descriptor_table_exiting(intercept_tables)
                .into_bits(),
        );

        // Update MSR intercepts. We only need to update those that are allowed
        // to be passed through, as the default otherwise is to always intercept.
        // See [`MSR_ALLOWED_READ_WRITE`].
        this.runner.set_msr_bit(
            vtl,
            x86defs::X86X_MSR_S_CET,
            true,
            intercept_control.msr_scet_write(),
        );
        this.runner.set_msr_bit(
            vtl,
            x86defs::X86X_MSR_PL0_SSP,
            true,
            intercept_control.msr_pls_ssp_write(),
        );
        this.runner.set_msr_bit(
            vtl,
            x86defs::X86X_MSR_PL1_SSP,
            true,
            intercept_control.msr_pls_ssp_write(),
        );
        this.runner.set_msr_bit(
            vtl,
            x86defs::X86X_MSR_PL2_SSP,
            true,
            intercept_control.msr_pls_ssp_write(),
        );
        this.runner.set_msr_bit(
            vtl,
            x86defs::X86X_MSR_PL3_SSP,
            true,
            intercept_control.msr_pls_ssp_write(),
        );
        this.runner.set_msr_bit(
            vtl,
            x86defs::X86X_MSR_INTERRUPT_SSP_TABLE_ADDR,
            true,
            intercept_control.msr_pls_ssp_write(),
        );
    }

    fn is_interrupt_pending(
        this: &mut UhProcessor<'_, Self>,
        vtl: GuestVtl,
        check_rflags: bool,
        dev: &impl CpuIo,
    ) -> bool {
        let backing_vtl = &this.backing.vtls[vtl];
        if backing_vtl.interruption_information.valid()
            && backing_vtl.interruption_information.interruption_type() == INTERRUPT_TYPE_NMI
        {
            return true;
        }

        let (vector, ppr) = if this.backing.cvm.lapics[vtl].lapic.is_offloaded() {
            let vector = backing_vtl.private_regs.rvi;
            let ppr = std::cmp::max(
                backing_vtl.private_regs.svi.into(),
                this.runner.tdx_apic_page(vtl).tpr.value,
            );
            (vector, ppr)
        } else {
            let lapic = &mut this.backing.cvm.lapics[vtl].lapic;
            let vector = lapic.next_irr().unwrap_or(0);
            let ppr = lapic
                .access(&mut TdxApicClient {
                    partition: this.partition,
                    apic_page: this.runner.tdx_apic_page_mut(vtl),
                    dev,
                    vmtime: &this.vmtime,
                    vtl,
                })
                .get_ppr();
            (vector, ppr)
        };
        let vector_priority = (vector as u32) >> 4;
        let ppr_priority = ppr >> 4;

        if vector_priority <= ppr_priority {
            return false;
        }

        if check_rflags && !RFlags::from_bits(backing_vtl.private_regs.rflags).interrupt_enable() {
            return false;
        }

        let interruptibility: Interruptibility = this
            .runner
            .read_vmcs32(vtl, VmcsField::VMX_VMCS_GUEST_INTERRUPTIBILITY)
            .into();

        if interruptibility.blocked_by_sti() || interruptibility.blocked_by_movss() {
            return false;
        }

        true
    }

    fn untrusted_synic_mut(&mut self) -> Option<&mut ProcessorSynic> {
        self.untrusted_synic.as_mut()
    }

    fn update_deadline(this: &mut UhProcessor<'_, Self>, ref_time_now: u64, next_ref_time: u64) {
        this.shared
            .guest_timer
            .update_deadline(this, ref_time_now, next_ref_time);
    }

    fn clear_deadline(this: &mut UhProcessor<'_, Self>) {
        this.shared.guest_timer.clear_deadline(this);
    }
}

/// Partition-wide shared data for TDX VPs.
#[derive(Inspect)]
pub struct TdxBackedShared {
    #[inspect(flatten)]
    pub(crate) cvm: UhCvmPartitionState,
    /// The synic state used for untrusted SINTs, that is, the SINTs for which
    /// the guest thinks it is interacting directly with the untrusted
    /// hypervisor via an architecture-specific interface.
    pub(crate) untrusted_synic: Option<GlobalSynic>,
    flush_state: VtlArray<TdxPartitionFlushState, 2>,
    #[inspect(iter_by_index)]
    active_vtl: Vec<AtomicU8>,
    /// CR4 bits that the guest is allowed to set to 1.
    cr4_allowed_bits: u64,
    /// Accessor for managing lower VTL timer deadlines.
    #[inspect(skip)]
    guest_timer: Box<dyn hardware_cvm::HardwareIsolatedGuestTimer<TdxBacked>>,
}

impl TdxBackedShared {
    pub(crate) fn new(
        partition_params: &UhPartitionNewParams<'_>,
        params: BackingSharedParams<'_>,
    ) -> Result<Self, crate::Error> {
        // Create a second synic to fully manage the untrusted SINTs
        // here. At time of writing, the hypervisor does not support
        // sharing the untrusted SINTs with the TDX L1. Even if it did,
        // performance would be poor for cases where the L1 implements
        // high-performance devices.
        let untrusted_synic = (partition_params.handle_synic && !partition_params.hide_isolation)
            .then(|| GlobalSynic::new(partition_params.topology.vp_count()));

        // TODO TDX: Consider just using MSR kernel module instead of explicit ioctl.
        let cr4_fixed1 = params.hcl.read_vmx_cr4_fixed1();
        let cr4_allowed_bits =
            (ShadowedRegister::Cr4.guest_owned_mask() | X64_CR4_MCE) & cr4_fixed1;

        let cvm = params.cvm_state.unwrap();

        // Configure timer interface for lower VTLs.
        let guest_timer: Box<dyn hardware_cvm::HardwareIsolatedGuestTimer<TdxBacked>> =
            match params.lower_vtl_timer_virt_available {
                true => {
                    // Use TDX L2-VM TSC deadline timer service. Calculate scale factor
                    // for fixed-point conversion from 100ns to TSC units.
                    let tsc_frequency = get_tsc_frequency(IsolationType::Tdx).unwrap();
                    const NUM_100NS_IN_SEC: u128 = 10_000_000;
                    let tsc_scale_100ns = ((tsc_frequency as u128) << 64) / NUM_100NS_IN_SEC;

                    tracing::info!(CVM_ALLOWED, "enabling TDX L2-VM TSC deadline timer service");

                    Box::new(TdxTscDeadlineService { tsc_scale_100ns })
                }
                false => {
                    // Fall back to [`VmTime`] interface.
                    Box::new(hardware_cvm::VmTimeGuestTimer)
                }
            };

        Ok(Self {
            untrusted_synic,
            flush_state: VtlArray::from_fn(|_| TdxPartitionFlushState::new()),
            cvm,
            // VPs start in VTL 2.
            active_vtl: std::iter::repeat_n(2, partition_params.topology.vp_count() as usize)
                .map(AtomicU8::new)
                .collect(),
            cr4_allowed_bits,
            guest_timer,
        })
    }

    /// Get the default guest host mask for the specified register.
    fn cr_guest_host_mask(&self, reg: ShadowedRegister) -> u64 {
        match reg {
            ShadowedRegister::Cr0 => {
                !ShadowedRegister::Cr0.guest_owned_mask() | X64_CR0_PE | X64_CR0_PG
            }
            ShadowedRegister::Cr4 => {
                !(ShadowedRegister::Cr4.guest_owned_mask() & self.cr4_allowed_bits)
            }
        }
    }
}

impl TdxBacked {
    /// Gets the number of pages that will be allocated from the shared page pool
    /// for each CPU.
    pub fn shared_pages_required_per_cpu() -> u64 {
        UhDirectOverlay::Count as u64
    }
}

// The memory used to back the untrusted synic is not guest-visible, but rather
// is allocated from our shared pool. Therefore it does not need to go through
// the normal memory protections path.
struct UntrustedSynicVtlProts<'a>(&'a GuestMemory);

impl hv1_emulator::VtlProtectAccess for UntrustedSynicVtlProts<'_> {
    fn check_modify_and_lock_overlay_page(
        &mut self,
        gpn: u64,
        _check_perms: hvdef::HvMapGpaFlags,
        _new_perms: Option<hvdef::HvMapGpaFlags>,
    ) -> Result<guestmem::LockedPages, HvError> {
        self.0
            .lock_gpns(false, &[gpn])
            .map_err(|_| HvError::OperationFailed)
    }

    fn unlock_overlay_page(&mut self, _gpn: u64) -> Result<(), HvError> {
        Ok(())
    }
}

#[expect(private_interfaces)]
impl BackingPrivate for TdxBacked {
    type HclBacking<'tdx> = Tdx<'tdx>;
    type Shared = TdxBackedShared;
    type EmulationCache = TdxEmulationCache;

    fn shared(shared: &BackingShared) -> &Self::Shared {
        let BackingShared::Tdx(shared) = shared else {
            unreachable!()
        };
        shared
    }

    fn new(
        params: super::BackingParams<'_, '_, Self>,
        shared: &TdxBackedShared,
    ) -> Result<Self, crate::Error> {
        // TODO TDX: ssp is for shadow stack
        // TODO TDX: direct overlay like snp?
        // TODO TDX: lapic / APIC setup?
        // TODO TDX: see ValInitializeVplc
        // TODO TDX: XCR_XFMEM setup?

        // Turn on MBEC for just VTL 0.
        params.runner.write_vmcs32(
            GuestVtl::Vtl0,
            VmcsField::VMX_VMCS_SECONDARY_PROCESSOR_CONTROLS,
            SecondaryProcessorControls::new()
                .with_mode_based_execute_control(true)
                .into(),
            SecondaryProcessorControls::new()
                .with_mode_based_execute_control(true)
                .into(),
        );

        let controls = TdxL2Ctls::new()
            // Configure L2 controls to permit shared memory.
            .with_enable_shared_ept(!shared.cvm.hide_isolation)
            // If the synic is to be managed by the hypervisor, then enable TDVMCALLs.
            .with_enable_tdvmcall(shared.untrusted_synic.is_none() && !shared.cvm.hide_isolation);

        params
            .runner
            .set_l2_ctls(GuestVtl::Vtl0, controls)
            .map_err(crate::Error::FailedToSetL2Ctls)?;

        for vtl in [GuestVtl::Vtl0, GuestVtl::Vtl1] {
            // Set guest/host masks for CR0 and CR4. These enable shadowing these
            // registers since TDX requires certain bits to be set at all times.
            let initial_cr0 = params
                .runner
                .read_vmcs64(vtl, VmcsField::VMX_VMCS_GUEST_CR0);
            assert_eq!(initial_cr0, X64_CR0_PE | X64_CR0_NE);

            // N.B. CR0.PE and CR0.PG are guest owned but still intercept when they
            // are changed for caching purposes and to ensure EFER is managed
            // properly due to the need to change execution state.
            params.runner.write_vmcs64(
                vtl,
                VmcsField::VMX_VMCS_CR0_READ_SHADOW,
                !0,
                X64_CR0_PE | X64_CR0_NE,
            );
            params.runner.write_vmcs64(
                vtl,
                VmcsField::VMX_VMCS_CR0_GUEST_HOST_MASK,
                !0,
                shared.cr_guest_host_mask(ShadowedRegister::Cr0),
            );

            let initial_cr4 = params
                .runner
                .read_vmcs64(vtl, VmcsField::VMX_VMCS_GUEST_CR4);
            assert_eq!(initial_cr4, X64_CR4_MCE | X64_CR4_VMXE);

            params
                .runner
                .write_vmcs64(vtl, VmcsField::VMX_VMCS_CR4_READ_SHADOW, !0, 0);
            params.runner.write_vmcs64(
                vtl,
                VmcsField::VMX_VMCS_CR4_GUEST_HOST_MASK,
                !0,
                shared.cr_guest_host_mask(ShadowedRegister::Cr4),
            );

            // Configure the MSR bitmap for this VP. Since the default MSR bitmap
            // is set to intercept everything only the MSRs that we want to allow
            // to passthrough need to be set.
            for msr in MSR_ALLOWED_READ {
                params.runner.set_msr_bit(vtl, *msr, false, false);
            }
            for msr in MSR_ALLOWED_READ_WRITE {
                params.runner.set_msr_bit(vtl, *msr, false, false);
                params.runner.set_msr_bit(vtl, *msr, true, false);
            }

            // Set the exception bitmap.
            if params.partition.intercept_debug_exceptions {
                if cfg!(feature = "gdb") {
                    let initial_exception_bitmap = params
                        .runner
                        .read_vmcs32(vtl, VmcsField::VMX_VMCS_EXCEPTION_BITMAP);

                    let exception_bitmap =
                        initial_exception_bitmap | (1 << x86defs::Exception::DEBUG.0);

                    params.runner.write_vmcs32(
                        vtl,
                        VmcsField::VMX_VMCS_EXCEPTION_BITMAP,
                        !0,
                        exception_bitmap,
                    );
                } else {
                    return Err(super::Error::InvalidDebugConfiguration);
                }
            }
        }

        let flush_page = shared
            .cvm
            .private_dma_client
            .allocate_dma_buffer(HV_PAGE_SIZE as usize)
            .map_err(crate::Error::AllocateTlbFlushPage)?;

        let untrusted_synic = shared
            .untrusted_synic
            .as_ref()
            .map(|synic| synic.add_vp(params.vp_info.base.vp_index));

        Ok(Self {
            vtls: VtlArray::from_fn(|vtl| {
                let vtl: GuestVtl = vtl.try_into().unwrap();
                TdxVtl {
                    efer: params
                        .runner
                        .read_vmcs64(vtl, VmcsField::VMX_VMCS_GUEST_EFER),
                    cr0: VirtualRegister::new(
                        ShadowedRegister::Cr0,
                        vtl,
                        params
                            .runner
                            .read_vmcs64(vtl, VmcsField::VMX_VMCS_GUEST_CR0),
                        !0,
                    ),
                    cr4: VirtualRegister::new(
                        ShadowedRegister::Cr4,
                        vtl,
                        params
                            .runner
                            .read_vmcs64(vtl, VmcsField::VMX_VMCS_GUEST_CR4),
                        shared.cr4_allowed_bits,
                    ),
                    msr_cstar: 0,
                    tpr_threshold: 0,
                    processor_controls: params
                        .runner
                        .read_vmcs32(vtl, VmcsField::VMX_VMCS_PROCESSOR_CONTROLS)
                        .into(),
                    interruption_information: Default::default(),
                    exception_error_code: 0,
                    interruption_set: false,
                    flush_state: TdxFlushState::new(),
                    private_regs: TdxPrivateRegs::new(vtl),
                    enter_stats: Default::default(),
                    exit_stats: Default::default(),
                }
            }),
            untrusted_synic,
            eoi_exit_bitmap: [0; 4],
            flush_page,
            cvm: UhCvmVpState::new(
                &shared.cvm,
                params.partition,
                params.vp_info,
                UhDirectOverlay::Count as usize,
            )?,
            tsc_deadline_state: shared
                .guest_timer
                .is_hardware_virtualized()
                .then(TdxTscDeadline::default),
        })
    }

    type StateAccess<'p, 'a>
        = UhVpStateAccess<'a, 'p, Self>
    where
        Self: 'a + 'p,
        'p: 'a;

    fn access_vp_state<'a, 'p>(
        this: &'a mut UhProcessor<'p, Self>,
        vtl: GuestVtl,
    ) -> Self::StateAccess<'p, 'a> {
        UhVpStateAccess::new(this, vtl)
    }

    fn init(this: &mut UhProcessor<'_, Self>) {
        // Configure the synic direct overlays.
        // So far, only VTL 0 is using these (for VMBus).
        let pfns = &this.backing.cvm.direct_overlay_handle.pfns();
        let reg = |gpn| {
            u64::from(
                HvSynicSimpSiefp::new()
                    .with_base_gpn(gpn)
                    .with_enabled(true),
            )
        };

        let values: &[(HvX64RegisterName, u64); 2] = &[
            (
                HvX64RegisterName::Sifp,
                reg(pfns[UhDirectOverlay::Sifp as usize]),
            ),
            (
                HvX64RegisterName::Sipp,
                reg(pfns[UhDirectOverlay::Sipp as usize]),
            ),
        ];

        let reg_count = if let Some(synic) = &mut this.backing.untrusted_synic {
            let prot_access = &mut UntrustedSynicVtlProts(&this.partition.gm[GuestVtl::Vtl0]);

            synic
                .set_simp(reg(pfns[UhDirectOverlay::Sipp as usize]), prot_access)
                .unwrap();
            synic
                .set_siefp(reg(pfns[UhDirectOverlay::Sifp as usize]), prot_access)
                .unwrap();
            // Set the SIEFP in the hypervisor so that the hypervisor can
            // directly signal synic events. Don't set the SIMP, since the
            // message page is owned by the paravisor.
            1
        } else {
            2
        };

        this.runner
            .set_vp_registers_hvcall(Vtl::Vtl0, &values[..reg_count])
            .expect("set_vp_registers hypercall for direct overlays should succeed");

        // Enable APIC offload by default for VTL 0.
        this.set_apic_offload(GuestVtl::Vtl0, true);
        this.backing.cvm.lapics[GuestVtl::Vtl0]
            .lapic
            .enable_offload();

        // But disable it for VTL 1.
        this.set_apic_offload(GuestVtl::Vtl1, false);

        // Initialize registers to the reset state, since this may be different
        // than what's on the VMCS and is certainly different than what's in the
        // VP enter and private register state (which was mostly zero
        // initialized).
        for vtl in [GuestVtl::Vtl0, GuestVtl::Vtl1] {
            let registers = Registers::at_reset(&this.partition.caps, &this.inner.vp_info);

            let mut state = this.access_state(vtl.into());
            state
                .set_registers(&registers)
                .expect("Resetting to architectural state should succeed");

            state.commit().expect("committing state should succeed");
        }

        // FX regs and XMM registers are zero-initialized by the kernel. Set
        // them to the arch default.
        *this.runner.fx_state_mut() =
            vp::Xsave::at_reset(&this.partition.caps, &this.inner.vp_info).fxsave();
    }

    async fn run_vp(
        this: &mut UhProcessor<'_, Self>,
        dev: &impl CpuIo,
        _stop: &mut virt::StopVp<'_>,
    ) -> Result<(), VpHaltReason> {
        this.run_vp_tdx(dev).await
    }

    fn poll_apic(this: &mut UhProcessor<'_, Self>, vtl: GuestVtl, scan_irr: bool) {
        if !this.try_poll_apic(vtl, scan_irr) {
            tracing::info!(CVM_ALLOWED, "disabling APIC offload due to auto EOI");
            let page = this.runner.tdx_apic_page_mut(vtl);
            let (irr, isr) = pull_apic_offload(page);

            this.backing.cvm.lapics[vtl]
                .lapic
                .disable_offload(&irr, &isr);
            this.set_apic_offload(vtl, false);
            this.try_poll_apic(vtl, false);
        }
    }

    fn request_extint_readiness(_this: &mut UhProcessor<'_, Self>) {
        unreachable!("extint managed through software apic")
    }

    fn request_untrusted_sint_readiness(this: &mut UhProcessor<'_, Self>, sints: u16) {
        if let Some(synic) = &mut this.backing.untrusted_synic {
            synic.request_sint_readiness(sints);
        } else {
            tracelimit::error_ratelimited!(CVM_ALLOWED, "untrusted synic is not configured");
        }
    }

    fn hv(&self, vtl: GuestVtl) -> Option<&ProcessorVtlHv> {
        Some(&self.cvm.hv[vtl])
    }

    fn hv_mut(&mut self, vtl: GuestVtl) -> Option<&mut ProcessorVtlHv> {
        Some(&mut self.cvm.hv[vtl])
    }

    fn handle_vp_start_enable_vtl_wake(this: &mut UhProcessor<'_, Self>, vtl: GuestVtl) {
        this.hcvm_handle_vp_start_enable_vtl(vtl)
    }

    fn vtl1_inspectable(this: &UhProcessor<'_, Self>) -> bool {
        this.hcvm_vtl1_inspectable()
    }

    fn process_interrupts(
        this: &mut UhProcessor<'_, Self>,
        scan_irr: VtlArray<bool, 2>,
        first_scan_irr: &mut bool,
        dev: &impl CpuIo,
    ) -> bool {
        this.cvm_process_interrupts(scan_irr, first_scan_irr, dev)
    }
}

impl UhProcessor<'_, TdxBacked> {
    /// Returns `Ok(false)` if the APIC offload needs to be disabled and the
    /// poll retried.
    fn try_poll_apic(&mut self, vtl: GuestVtl, scan_irr: bool) -> bool {
        let mut scan = TdxApicScanner {
            processor_controls: self.backing.vtls[vtl]
                .processor_controls
                .with_nmi_window_exiting(false)
                .with_interrupt_window_exiting(false),
            vp: self,
            tpr_threshold: 0,
        };

        // TODO TDX: filter proxy IRRs by setting the `proxy_irr_blocked` field of the run page
        hardware_cvm::apic::poll_apic_core(&mut scan, vtl, scan_irr);

        let TdxApicScanner {
            vp: _,
            processor_controls: new_processor_controls,
            tpr_threshold: new_tpr_threshold,
        } = scan;

        // Interrupts are ignored while waiting for SIPI.
        if self.backing.cvm.lapics[vtl].activity != MpState::WaitForSipi
            && self.backing.vtls[vtl].tpr_threshold != new_tpr_threshold
        {
            tracing::trace!(new_tpr_threshold, ?vtl, "setting tpr threshold");
            self.runner.write_vmcs32(
                vtl,
                VmcsField::VMX_VMCS_TPR_THRESHOLD,
                !0,
                new_tpr_threshold.into(),
            );
            self.backing.vtls[vtl].tpr_threshold = new_tpr_threshold;
        }

        if self.backing.vtls[vtl].processor_controls != new_processor_controls {
            tracing::trace!(?new_processor_controls, ?vtl, "requesting window change");
            self.runner.write_vmcs32(
                vtl,
                VmcsField::VMX_VMCS_PROCESSOR_CONTROLS,
                !0,
                new_processor_controls.into(),
            );
            self.backing.vtls[vtl].processor_controls = new_processor_controls;
        }

        // Offloading and proxying is only done with VTL 0 today.
        if vtl == GuestVtl::Vtl0 {
            let mut update_rvi = false;
            let r: Result<(), OffloadNotSupported> = self.backing.cvm.lapics[vtl]
                .lapic
                .push_to_offload(|irr, isr, tmr| {
                    let apic_page = self.runner.tdx_apic_page_mut(vtl);

                    for (((irr, page_irr), isr), page_isr) in irr
                        .iter()
                        .zip(&mut apic_page.irr)
                        .zip(isr)
                        .zip(&mut apic_page.isr)
                    {
                        page_irr.value |= *irr;
                        page_isr.value |= *isr;
                    }

                    // Update SVI and RVI.
                    let svi = top_vector(&apic_page.isr);
                    self.backing.vtls[vtl].private_regs.svi = svi;
                    update_rvi = true;

                    // Ensure the EOI exit bitmap is up to date.
                    let fields = [
                        VmcsField::VMX_VMCS_EOI_EXIT_0,
                        VmcsField::VMX_VMCS_EOI_EXIT_1,
                        VmcsField::VMX_VMCS_EOI_EXIT_2,
                        VmcsField::VMX_VMCS_EOI_EXIT_3,
                    ];
                    for ((&field, eoi_exit), (i, tmr)) in fields
                        .iter()
                        .zip(&mut self.backing.eoi_exit_bitmap)
                        .zip(tmr.chunks_exact(2).enumerate())
                    {
                        let tmr = tmr[0] as u64 | ((tmr[1] as u64) << 32);
                        if *eoi_exit != tmr {
                            self.runner.write_vmcs64(vtl, field, !0, tmr);
                            *eoi_exit = tmr;
                            // The kernel driver supports some common APIC functionality (ICR writes,
                            // interrupt injection). When the kernel driver handles an interrupt, it
                            // must know if that interrupt was previously level-triggered. Otherwise,
                            // the EOI will be incorrectly treated as level-triggered. We keep a copy
                            // of the tmr in the kernel so it knows when this scenario occurs.
                            self.runner.proxy_irr_exit_mut_vtl0()[i * 2] = tmr as u32;
                            self.runner.proxy_irr_exit_mut_vtl0()[i * 2 + 1] = (tmr >> 32) as u32;
                        }
                    }
                });

            if let Err(OffloadNotSupported) = r {
                // APIC needs offloading to be disabled to support auto-EOI. The caller
                // will disable offload and try again.
                return false;
            }

            if update_rvi {
                let page = self.runner.tdx_apic_page_mut(vtl);
                let rvi = top_vector(&page.irr);
                self.backing.vtls[vtl].private_regs.rvi = rvi;
            }
        }

        // If there is a pending interrupt, clear the halted and idle state.
        if (self.backing.cvm.lapics[vtl].activity != MpState::Running)
            && self.backing.cvm.lapics[vtl].lapic.is_offloaded()
            && self.backing.vtls[vtl].private_regs.rvi != 0
        {
            // To model a non-virtualized processor, we should only do this if
            // TPR and IF and interrupt shadow allow. However, fetching the
            // interrupt shadow state is expensive (tdcall). This shouldn't
            // matter much, because real guests don't issue hlt while in
            // interrupt shadow or with interrupts disabled or with a non-zero
            // TPR.
            //
            // Note that the processor will not actually inject the interrupt
            // until conditions hold. So, unless the guest fails to loop around
            // and hlt again (which we already treat as a guest bug, since
            // Hyper-V in general does not guarantee hlt will stick until an
            // interrupt is pending), at worst this will just burn some CPU.
            self.backing.cvm.lapics[vtl].activity = MpState::Running;
        }

        true
    }

    fn access_apic_without_offload<R>(
        &mut self,
        vtl: GuestVtl,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let offloaded = self.backing.cvm.lapics[vtl].lapic.is_offloaded();
        if offloaded {
            let (irr, isr) = pull_apic_offload(self.runner.tdx_apic_page_mut(vtl));
            self.backing.cvm.lapics[vtl]
                .lapic
                .disable_offload(&irr, &isr);
        }
        let r = f(self);
        if offloaded {
            self.backing.cvm.lapics[vtl].lapic.enable_offload();
        }
        r
    }

    fn set_apic_offload(&mut self, vtl: GuestVtl, offload: bool) {
        // Update the APIC portion of the MSR bitmap.
        let offload_bitmap = if offload {
            (1 << x86defs::apic::ApicRegister::TPR.0)
                | (1 << x86defs::apic::ApicRegister::EOI.0)
                | (1 << x86defs::apic::ApicRegister::SELF_IPI.0)
        } else {
            0
        };
        // Once for read and once for write.
        for offset in [0, 0x100] {
            self.runner
                .write_msr_bitmap(vtl, offset + X2APIC_MSR_BASE / 64, !0, !offload_bitmap);
        }

        // Update virtual-interrupt delivery.
        self.runner.write_vmcs32(
            vtl,
            VmcsField::VMX_VMCS_SECONDARY_PROCESSOR_CONTROLS,
            SecondaryProcessorControls::new()
                .with_virtual_interrupt_delivery(true)
                .into(),
            SecondaryProcessorControls::new()
                .with_virtual_interrupt_delivery(offload)
                .into(),
        );

        // Clear any pending external interrupt when enabling the APIC offload.
        if offload
            && self.backing.vtls[vtl]
                .interruption_information
                .interruption_type()
                == INTERRUPT_TYPE_EXTERNAL
        {
            self.backing.vtls[vtl]
                .interruption_information
                .set_valid(false);
        }
    }
}

struct TdxApicScanner<'a, 'b> {
    vp: &'a mut UhProcessor<'b, TdxBacked>,
    processor_controls: ProcessorControls,
    tpr_threshold: u8,
}

impl<'b> hardware_cvm::apic::ApicBacking<'b, TdxBacked> for TdxApicScanner<'_, 'b> {
    fn vp(&mut self) -> &mut UhProcessor<'b, TdxBacked> {
        self.vp
    }

    fn handle_interrupt(&mut self, vtl: GuestVtl, vector: u8) {
        // Exit idle when an interrupt is received, regardless of IF
        if self.vp.backing.cvm.lapics[vtl].activity == MpState::Idle {
            self.vp.backing.cvm.lapics[vtl].activity = MpState::Running;
        }
        // If there is a higher-priority pending event of some kind, then
        // just request an exit after it has resolved, after which we will
        // try again.
        if self.vp.backing.vtls[vtl].interruption_information.valid()
            && self.vp.backing.vtls[vtl]
                .interruption_information
                .interruption_type()
                != INTERRUPT_TYPE_EXTERNAL
        {
            self.processor_controls.set_interrupt_window_exiting(true);
            return;
        }

        // Ensure the interrupt is not blocked by RFLAGS.IF or interrupt shadow.
        let interruptibility: Interruptibility = self
            .vp
            .runner
            .read_vmcs32(vtl, VmcsField::VMX_VMCS_GUEST_INTERRUPTIBILITY)
            .into();

        let rflags = RFlags::from(self.vp.backing.vtls[vtl].private_regs.rflags);
        if !rflags.interrupt_enable()
            || interruptibility.blocked_by_sti()
            || interruptibility.blocked_by_movss()
        {
            self.processor_controls.set_interrupt_window_exiting(true);
            return;
        }

        let priority = vector >> 4;
        let apic = self.vp.runner.tdx_apic_page(vtl);
        if (apic.tpr.value as u8 >> 4) >= priority {
            self.tpr_threshold = priority;
            return;
        }

        self.vp.backing.vtls[vtl].interruption_information = InterruptionInformation::new()
            .with_valid(true)
            .with_vector(vector)
            .with_interruption_type(INTERRUPT_TYPE_EXTERNAL);

        self.vp.backing.cvm.lapics[vtl].activity = MpState::Running;
    }

    fn handle_nmi(&mut self, vtl: GuestVtl) {
        // Exit idle when an interrupt is received, regardless of IF
        // TODO: Investigate lifting more activity management into poll_apic_core
        if self.vp.backing.cvm.lapics[vtl].activity == MpState::Idle {
            self.vp.backing.cvm.lapics[vtl].activity = MpState::Running;
        }
        // If there is a higher-priority pending event of some kind, then
        // just request an exit after it has resolved, after which we will
        // try again.
        if self.vp.backing.vtls[vtl].interruption_information.valid()
            && self.vp.backing.vtls[vtl]
                .interruption_information
                .interruption_type()
                != INTERRUPT_TYPE_EXTERNAL
        {
            self.processor_controls.set_nmi_window_exiting(true);
            return;
        }

        let interruptibility: Interruptibility = self
            .vp
            .runner
            .read_vmcs32(vtl, VmcsField::VMX_VMCS_GUEST_INTERRUPTIBILITY)
            .into();

        if interruptibility.blocked_by_nmi()
            || interruptibility.blocked_by_sti()
            || interruptibility.blocked_by_movss()
        {
            self.processor_controls.set_nmi_window_exiting(true);
            return;
        }

        self.vp.backing.vtls[vtl].interruption_information = InterruptionInformation::new()
            .with_valid(true)
            .with_vector(2)
            .with_interruption_type(INTERRUPT_TYPE_NMI);

        self.vp.backing.cvm.lapics[vtl].activity = MpState::Running;
    }

    fn handle_sipi(&mut self, vtl: GuestVtl, cs: SegmentRegister) {
        self.vp.write_segment(vtl, TdxSegmentReg::Cs, cs).unwrap();
        self.vp.backing.vtls[vtl].private_regs.rip = 0;
        self.vp.backing.cvm.lapics[vtl].activity = MpState::Running;
    }
}

impl UhProcessor<'_, TdxBacked> {
    async fn run_vp_tdx(&mut self, dev: &impl CpuIo) -> Result<(), VpHaltReason> {
        let next_vtl = self.backing.cvm.exit_vtl;

        if self.backing.vtls[next_vtl].interruption_information.valid() {
            tracing::trace!(
                vector = self.backing.vtls[next_vtl]
                    .interruption_information
                    .vector(),
                vp_index = self.vp_index().index(),
                ?next_vtl,
                "injecting interrupt"
            );

            self.runner.write_vmcs32(
                next_vtl,
                VmcsField::VMX_VMCS_ENTRY_INTERRUPT_INFO,
                !0,
                self.backing.vtls[next_vtl].interruption_information.into(),
            );
            if self.backing.vtls[next_vtl]
                .interruption_information
                .deliver_error_code()
            {
                self.runner.write_vmcs32(
                    next_vtl,
                    VmcsField::VMX_VMCS_ENTRY_EXCEPTION_ERROR_CODE,
                    !0,
                    self.backing.vtls[next_vtl].exception_error_code,
                );
            }
            self.backing.vtls[next_vtl].interruption_set = true;
        } else if self.backing.vtls[next_vtl].interruption_set {
            self.runner
                .write_vmcs32(next_vtl, VmcsField::VMX_VMCS_ENTRY_INTERRUPT_INFO, !0, 0);
            self.backing.vtls[next_vtl].interruption_set = false;
        }

        // We're about to return to a lower VTL, so set active_vtl for other VPs,
        // do any pending flushes, unlock our TLB locks, and wait for any others
        // we're supposed to.

        // active_vtl needs SeqCst ordering here in order to correctly synchronize
        // access with the TLB address flush list. We need to ensure that, when
        // other VPs are adding entries to the list, they always observe the
        // correct lower active VTL. Otherwise they might choose to not send this
        // VP a wake, leading to a stall, until this VP happens to exit to VTL 2 again.
        //
        // This does technically leave open a small window for potential spurious
        // wakes, but that's preferable, and will cause no problems besides a
        // small amount of time waste.
        self.shared.active_vtl[self.vp_index().index() as usize]
            .store(next_vtl as u8, Ordering::SeqCst);

        self.do_tlb_flush(next_vtl);
        self.unlock_tlb_lock(Vtl::Vtl2);
        let tlb_halt = self.should_halt_for_tlb_unlock(next_vtl);

        // If we are halted in the kernel due to hlt or idle, and we receive an interrupt
        // we'd like to unhalt, inject the interrupt, and resume vtl0 without returning to
        // user-mode. To enable this, the kernel must know why are are halted
        let activity = self.backing.cvm.lapics[next_vtl].activity;
        let kernel_known_state =
            matches!(activity, MpState::Running | MpState::Halted | MpState::Idle);
        let halted_other = tlb_halt || !kernel_known_state;

        self.runner
            .set_halted(activity != MpState::Running || tlb_halt);

        // Turn on kernel interrupt handling if possible. This will cause the
        // kernel to handle some exits internally, without returning to user
        // mode, to improve performance.
        //
        // Do not do this if there is a pending interruption, since we need to
        // run code on the next exit to clear it. If we miss this opportunity,
        // we will probably double-inject the interruption, wreaking havoc.
        //
        // Also do not do this if there is a pending TLB flush, since we need to
        // run code on the next exit to clear it. If we miss this opportunity,
        // we could double-inject the TLB flush unnecessarily.
        let offload_enabled = self.backing.cvm.lapics[next_vtl].lapic.can_offload_irr()
            && !self.backing.vtls[next_vtl].interruption_information.valid()
            && self.backing.vtls[next_vtl]
                .private_regs
                .vp_entry_flags
                .invd_translations()
                == 0;
        let x2apic_enabled = self.backing.cvm.lapics[next_vtl].lapic.x2apic_enabled();

        let offload_flags = hcl_intr_offload_flags::new()
            .with_offload_intr_inject(offload_enabled)
            .with_offload_x2apic(offload_enabled && x2apic_enabled)
            .with_halted_other(halted_other)
            .with_halted_hlt(activity == MpState::Halted)
            .with_halted_idle(activity == MpState::Idle);

        *self.runner.offload_flags_mut() = offload_flags;

        self.runner
            .write_private_regs(&self.backing.vtls[next_vtl].private_regs);

        let has_intercept = self
            .runner
            .run()
            .map_err(|e| dev.fatal_error(TdxRunVpError(e).into()))?;

        // TLB flushes can only target lower VTLs, so it is fine to use a relaxed
        // ordering here. The worst that can happen is some spurious wakes, due
        // to another VP observing that this VP is still in a lower VTL.
        self.shared.active_vtl[self.vp_index().index() as usize].store(2, Ordering::Relaxed);

        let entered_from_vtl = next_vtl;
        self.runner
            .read_private_regs(&mut self.backing.vtls[entered_from_vtl].private_regs);

        // Synchronize timer deadline state
        self.shared.guest_timer.sync_deadline_state(self);

        // Kernel offload may have set or cleared the halt/idle states
        if offload_enabled && kernel_known_state {
            let offload_flags = self.runner.offload_flags_mut();

            self.backing.cvm.lapics[entered_from_vtl].activity =
                match (offload_flags.halted_hlt(), offload_flags.halted_idle()) {
                    (false, false) => MpState::Running,
                    (true, false) => MpState::Halted,
                    (false, true) => MpState::Idle,
                    (true, true) => {
                        tracelimit::warn_ratelimited!(
                            CVM_ALLOWED,
                            "Kernel indicates VP is both halted and idle!"
                        );
                        activity
                    }
                };
        }

        if !has_intercept {
            return Ok(());
        }

        let exit_info = TdxExit(self.runner.tdx_vp_enter_exit_info());

        // Result codes above PENDING_INTERRUPT indicate the L2 was never entered.
        if exit_info.code().tdx_exit() >= TdCallResultCode::PENDING_INTERRUPT {
            self.backing.vtls[entered_from_vtl]
                .enter_stats
                .pending_intr
                .increment();
            return Ok(());
        }

        // Since the L2 was entered we can clear any TLB flush requests
        self.backing.vtls[entered_from_vtl]
            .private_regs
            .vp_entry_flags
            .set_invd_translations(0);

        // The L2 was entered, so process the exit.
        let stat = match exit_info.code().tdx_exit() {
            TdCallResultCode::SUCCESS => {
                &mut self.backing.vtls[entered_from_vtl].enter_stats.success
            }
            TdCallResultCode::L2_EXIT_HOST_ROUTED_ASYNC => {
                &mut self.backing.vtls[entered_from_vtl]
                    .enter_stats
                    .host_routed_async
            }
            TdCallResultCode::L2_EXIT_PENDING_INTERRUPT => {
                &mut self.backing.vtls[entered_from_vtl]
                    .enter_stats
                    .l2_exit_pending_intr
            }
            TdCallResultCode::L2_EXIT_HOST_ROUTED_TDVMCALL => {
                // This is expected, and means that the hypervisor completed a
                // TD.VMCALL from the L2 and has requested to resume the L2 to
                // the L1.
                //
                // There is nothing to do here.
                assert_eq!(
                    exit_info.code().vmx_exit(),
                    VmxExit::new().with_basic_reason(VmxExitBasic::TDCALL)
                );
                &mut self.backing.vtls[entered_from_vtl]
                    .enter_stats
                    .host_routed_td_vmcall
            }
            _ => panic!("unexpected tdx exit code {:?}", exit_info.code()),
        };

        stat.increment();
        self.handle_vmx_exit(dev, entered_from_vtl).await?;
        Ok(())
    }

    async fn handle_vmx_exit(
        &mut self,
        dev: &impl CpuIo,
        intercepted_vtl: GuestVtl,
    ) -> Result<(), VpHaltReason> {
        let exit_info = TdxExit(self.runner.tdx_vp_enter_exit_info());

        // First, check that the VM entry was even successful.
        let vmx_exit = exit_info.code().vmx_exit();
        if vmx_exit.vm_enter_failed() {
            return Err(self.handle_vm_enter_failed(dev, intercepted_vtl, vmx_exit));
        }

        let next_interruption = exit_info.idt_vectoring_info();

        // Acknowledge the APIC interrupt/NMI if it was delivered.
        if self.backing.vtls[intercepted_vtl]
            .interruption_information
            .valid()
            && (!next_interruption.valid()
                || self.backing.vtls[intercepted_vtl]
                    .interruption_information
                    .interruption_type()
                    != next_interruption.interruption_type())
        {
            match self.backing.vtls[intercepted_vtl]
                .interruption_information
                .interruption_type()
            {
                INTERRUPT_TYPE_EXTERNAL
                    if !self.backing.cvm.lapics[intercepted_vtl]
                        .lapic
                        .is_offloaded() =>
                {
                    // This must be a pending APIC interrupt. Acknowledge it.
                    tracing::trace!(
                        vector = self.backing.vtls[intercepted_vtl]
                            .interruption_information
                            .vector(),
                        "acknowledging interrupt"
                    );
                    self.backing.cvm.lapics[intercepted_vtl]
                        .lapic
                        .acknowledge_interrupt(
                            self.backing.vtls[intercepted_vtl]
                                .interruption_information
                                .vector(),
                        );
                }
                INTERRUPT_TYPE_NMI => {
                    // This must be a pending NMI.
                    tracing::debug!("acknowledging NMI");
                    self.backing.cvm.lapics[intercepted_vtl].nmi_pending = false;
                }
                _ => {}
            }
        }

        if self.backing.cvm.lapics[intercepted_vtl]
            .lapic
            .is_offloaded()
        {
            // It's possible with vAPIC that we take an exit in the window where
            // hardware has moved a bit from IRR to ISR, but has not injected
            // the interrupt into the guest. In this case, we need to track that
            // we must inject the interrupt before we return to the guest,
            // otherwise the interrupt will be lost and the guest left in a bad
            // state.
            //
            // TODO TDX: Unclear what kind of exits these would be, but they
            // should be spurious EPT exits. Can we validate or assert that
            // somehow? If we were to somehow call some other path which would
            // set interruption_information before we inject this one, we would
            // lose this interrupt.
            if next_interruption.valid() {
                tracing::debug!(
                    ?next_interruption,
                    vp_index = self.vp_index().index(),
                    "exit requires reinjecting interrupt"
                );
                self.backing.vtls[intercepted_vtl].interruption_information = next_interruption;
                self.backing.vtls[intercepted_vtl].exception_error_code =
                    exit_info.idt_vectoring_error_code();
                self.backing.vtls[intercepted_vtl]
                    .exit_stats
                    .needs_interrupt_reinject
                    .increment();
            } else {
                self.backing.vtls[intercepted_vtl].interruption_information = Default::default();
            }
        } else {
            // Ignore (and later recalculate) the next interruption if it is an
            // external interrupt or NMI, since it may change if the APIC state
            // changes.
            if next_interruption.valid()
                && !matches!(
                    next_interruption.interruption_type(),
                    INTERRUPT_TYPE_EXTERNAL | INTERRUPT_TYPE_NMI
                )
            {
                self.backing.vtls[intercepted_vtl].interruption_information = next_interruption;
                self.backing.vtls[intercepted_vtl].exception_error_code =
                    exit_info.idt_vectoring_error_code();
            } else {
                self.backing.vtls[intercepted_vtl].interruption_information = Default::default();
            }
        }

        let mut breakpoint_debug_exception = false;
        let stat = match vmx_exit.basic_reason() {
            VmxExitBasic::IO_INSTRUCTION => {
                let io_qual = ExitQualificationIo::from(exit_info.qualification() as u32);

                let len = match io_qual.access_size() {
                    IO_SIZE_8_BIT => 1,
                    IO_SIZE_16_BIT => 2,
                    IO_SIZE_32_BIT => 4,
                    _ => panic!(
                        "tdx module returned invalid io instr size {}",
                        io_qual.access_size()
                    ),
                };

                let port_access_protected = self.cvm_try_protect_io_port_access(
                    intercepted_vtl,
                    io_qual.port(),
                    io_qual.is_in(),
                    len,
                    io_qual.is_string(),
                    io_qual.rep_prefix(),
                );

                if !port_access_protected {
                    if io_qual.is_string() || io_qual.rep_prefix() {
                        // TODO GUEST VSM: consider changing the emulation path
                        // to also check for io port installation, mainly for
                        // handling rep instructions.

                        self.emulate(
                            dev,
                            self.backing.vtls[intercepted_vtl]
                                .interruption_information
                                .valid(),
                            intercepted_vtl,
                            TdxEmulationCache::default(),
                        )
                        .await?;
                    } else {
                        let mut rax = self.runner.tdx_enter_guest_gps()[TdxGp::RAX];
                        emulate_io(
                            self.inner.vp_info.base.vp_index,
                            !io_qual.is_in(),
                            io_qual.port(),
                            &mut rax,
                            len,
                            dev,
                        )
                        .await;
                        self.runner.tdx_enter_guest_gps_mut()[TdxGp::RAX] = rax;

                        self.advance_to_next_instruction(intercepted_vtl);
                    }
                }

                &mut self.backing.vtls[intercepted_vtl].exit_stats.io
            }
            VmxExitBasic::MSR_READ => {
                let msr = self.runner.tdx_enter_guest_gps()[TdxGp::RCX] as u32;

                let result = self.backing.cvm.lapics[intercepted_vtl]
                    .lapic
                    .access(&mut TdxApicClient {
                        partition: self.partition,
                        vmtime: &self.vmtime,
                        apic_page: self.runner.tdx_apic_page_mut(intercepted_vtl),
                        dev,
                        vtl: intercepted_vtl,
                    })
                    .msr_read(msr)
                    .or_else_if_unknown(|| self.read_msr_cvm(msr, intercepted_vtl))
                    .or_else_if_unknown(|| self.read_msr_tdx(msr, intercepted_vtl));

                let value = match result {
                    Ok(v) => Some(v),
                    Err(MsrError::Unknown) => {
                        tracelimit::warn_ratelimited!(CVM_ALLOWED, msr, "unknown tdx vm msr read");
                        Some(0)
                    }
                    Err(MsrError::InvalidAccess) => None,
                };

                let inject_gp = if let Some(value) = value {
                    let gps = self.runner.tdx_enter_guest_gps_mut();
                    gps[TdxGp::RAX] = (value as u32).into();
                    gps[TdxGp::RDX] = ((value >> 32) as u32).into();
                    false
                } else {
                    true
                };

                if inject_gp {
                    self.inject_gpf(intercepted_vtl);
                } else {
                    self.advance_to_next_instruction(intercepted_vtl);
                }
                &mut self.backing.vtls[intercepted_vtl].exit_stats.msr_read
            }
            VmxExitBasic::MSR_WRITE => {
                let gps = self.runner.tdx_enter_guest_gps();
                let msr = gps[TdxGp::RCX] as u32;
                let value =
                    (gps[TdxGp::RAX] as u32 as u64) | ((gps[TdxGp::RDX] as u32 as u64) << 32);

                if !self.cvm_try_protect_msr_write(intercepted_vtl, msr) {
                    let result = self.backing.cvm.lapics[intercepted_vtl]
                        .lapic
                        .access(&mut TdxApicClient {
                            partition: self.partition,
                            vmtime: &self.vmtime,
                            apic_page: self.runner.tdx_apic_page_mut(intercepted_vtl),
                            dev,
                            vtl: intercepted_vtl,
                        })
                        .msr_write(msr, value)
                        .or_else_if_unknown(|| self.write_msr_cvm(msr, value, intercepted_vtl))
                        .or_else_if_unknown(|| self.write_msr_tdx(msr, value, intercepted_vtl))
                        .or_else_if_unknown(|| {
                            // Sanity check
                            if MSR_ALLOWED_READ_WRITE.contains(&msr) {
                                unreachable!("intercepted a write to MSR {msr}, configured for passthrough by default, that wasn't registered for intercepts by a higher VTL");
                            }
                            Err(MsrError::Unknown)
                        });

                    let inject_gp = match result {
                        Ok(()) => false,
                        Err(MsrError::Unknown) => {
                            tracelimit::warn_ratelimited!(
                                CVM_ALLOWED,
                                msr,
                                "unknown tdx vm msr write"
                            );
                            tracelimit::warn_ratelimited!(
                                CVM_CONFIDENTIAL,
                                value,
                                "unknown tdx vm msr write"
                            );
                            false
                        }
                        Err(MsrError::InvalidAccess) => true,
                    };

                    if inject_gp {
                        self.inject_gpf(intercepted_vtl);
                    } else {
                        self.advance_to_next_instruction(intercepted_vtl);
                    }
                }
                &mut self.backing.vtls[intercepted_vtl].exit_stats.msr_write
            }
            VmxExitBasic::CPUID => {
                let gps = self.runner.tdx_enter_guest_gps();
                let leaf = gps[TdxGp::RAX] as u32;
                let subleaf = gps[TdxGp::RCX] as u32;
                let [eax, ebx, ecx, edx] = self.cvm_cpuid_result(intercepted_vtl, leaf, subleaf);
                let gps = self.runner.tdx_enter_guest_gps_mut();
                gps[TdxGp::RAX] = eax.into();
                gps[TdxGp::RBX] = ebx.into();
                gps[TdxGp::RCX] = ecx.into();
                gps[TdxGp::RDX] = edx.into();
                self.advance_to_next_instruction(intercepted_vtl);
                &mut self.backing.vtls[intercepted_vtl].exit_stats.cpuid
            }
            VmxExitBasic::VMCALL_INSTRUCTION => {
                if exit_info.cpl() != 0 {
                    self.inject_gpf(intercepted_vtl);
                } else {
                    let is_64bit = self.long_mode(intercepted_vtl);
                    let guest_memory = &self.partition.gm[intercepted_vtl];
                    let handler = UhHypercallHandler {
                        trusted: !self.cvm_partition().hide_isolation,
                        vp: &mut *self,
                        bus: dev,
                        intercepted_vtl,
                    };

                    UhHypercallHandler::TDX_DISPATCHER.dispatch(
                        guest_memory,
                        hv1_hypercall::X64RegisterIo::new(handler, is_64bit),
                    );
                }
                &mut self.backing.vtls[intercepted_vtl].exit_stats.vmcall
            }
            VmxExitBasic::HLT_INSTRUCTION => {
                self.backing.cvm.lapics[intercepted_vtl].activity = MpState::Halted;
                self.clear_interrupt_shadow(intercepted_vtl);
                self.advance_to_next_instruction(intercepted_vtl);
                &mut self.backing.vtls[intercepted_vtl].exit_stats.hlt
            }
            VmxExitBasic::CR_ACCESS => {
                let qual = CrAccessQualification::from(exit_info.qualification());
                let cr;
                let value;
                match qual.access_type() {
                    CR_ACCESS_TYPE_MOV_TO_CR => {
                        cr = qual.cr();
                        value = self.runner.tdx_enter_guest_gps()[qual.gp_register() as usize];
                    }
                    CR_ACCESS_TYPE_LMSW => {
                        cr = 0;
                        let cr0 = self.backing.vtls[intercepted_vtl].cr0.read(&self.runner);
                        // LMSW updates the low four bits only.
                        value = (qual.lmsw_source_data() as u64 & 0xf) | (cr0 & !0xf);
                    }
                    access_type => unreachable!("not registered for cr access type {access_type}"),
                }

                let cr = match cr {
                    0 => HvX64RegisterName::Cr0,
                    4 => HvX64RegisterName::Cr4,
                    _ => unreachable!("not registered for cr{cr} accesses"),
                };

                if !self.cvm_try_protect_secure_register_write(intercepted_vtl, cr, value) {
                    let r = match cr {
                        HvX64RegisterName::Cr0 => self.backing.vtls[intercepted_vtl]
                            .cr0
                            .write(value, &mut self.runner),
                        HvX64RegisterName::Cr4 => self.backing.vtls[intercepted_vtl]
                            .cr4
                            .write(value, &mut self.runner),
                        _ => unreachable!(),
                    };
                    if r.is_ok() {
                        self.update_execution_mode(intercepted_vtl);
                        self.advance_to_next_instruction(intercepted_vtl);
                    } else {
                        tracelimit::warn_ratelimited!(
                            CVM_ALLOWED,
                            ?cr,
                            value,
                            "failed to write cr"
                        );
                        self.inject_gpf(intercepted_vtl);
                    }
                }
                &mut self.backing.vtls[intercepted_vtl].exit_stats.cr_access
            }
            VmxExitBasic::XSETBV => {
                let gps = self.runner.tdx_enter_guest_gps();
                if let Some(value) =
                    hardware_cvm::validate_xsetbv_exit(hardware_cvm::XsetbvExitInput {
                        rax: gps[TdxGp::RAX],
                        rcx: gps[TdxGp::RCX],
                        rdx: gps[TdxGp::RDX],
                        cr4: self.backing.vtls[intercepted_vtl].cr4.read(&self.runner),
                        cpl: exit_info.cpl(),
                    })
                {
                    if !self.cvm_try_protect_secure_register_write(
                        intercepted_vtl,
                        HvX64RegisterName::Xfem,
                        value,
                    ) {
                        self.runner
                            .set_vp_register(intercepted_vtl, HvX64RegisterName::Xfem, value.into())
                            .unwrap();
                        self.advance_to_next_instruction(intercepted_vtl);
                    }
                } else {
                    self.inject_gpf(intercepted_vtl);
                }
                &mut self.backing.vtls[intercepted_vtl].exit_stats.xsetbv
            }
            VmxExitBasic::WBINVD_INSTRUCTION => {
                // Ask the kernel to flush the cache before issuing VP.ENTER.
                let no_invalidate = exit_info.qualification() != 0;
                if no_invalidate {
                    self.runner.tdx_vp_state_flags_mut().set_wbnoinvd(true);
                } else {
                    self.runner.tdx_vp_state_flags_mut().set_wbinvd(true);
                }

                self.advance_to_next_instruction(intercepted_vtl);
                &mut self.backing.vtls[intercepted_vtl].exit_stats.wbinvd
            }
            VmxExitBasic::EPT_VIOLATION => {
                let gpa = exit_info.gpa().expect("is EPT exit");
                let ept_info = VmxEptExitQualification::from(exit_info.qualification());
                // If this was an EPT violation while handling an iret, and
                // that iret cleared the NMI blocking state, restore it.
                if !next_interruption.valid() && ept_info.nmi_unmasking_due_to_iret() {
                    let mask = Interruptibility::new().with_blocked_by_nmi(true);
                    let value = Interruptibility::new().with_blocked_by_nmi(true);
                    let old_interruptibility: Interruptibility = self
                        .runner
                        .write_vmcs32(
                            intercepted_vtl,
                            VmcsField::VMX_VMCS_GUEST_INTERRUPTIBILITY,
                            mask.into(),
                            value.into(),
                        )
                        .into();
                    assert!(!old_interruptibility.blocked_by_nmi());
                } else {
                    let is_write = ept_info.access_mask() & 0b10 != 0;
                    if self.check_mem_fault(intercepted_vtl, gpa, is_write, ept_info) {
                        self.emulate(
                            dev,
                            self.backing.vtls[intercepted_vtl]
                                .interruption_information
                                .valid(),
                            intercepted_vtl,
                            TdxEmulationCache::default(),
                        )
                        .await?;
                    }
                }

                &mut self.backing.vtls[intercepted_vtl].exit_stats.ept_violation
            }
            VmxExitBasic::TPR_BELOW_THRESHOLD => {
                // Loop around to reevaluate the APIC.
                &mut self.backing.vtls[intercepted_vtl]
                    .exit_stats
                    .tpr_below_threshold
            }
            VmxExitBasic::INTERRUPT_WINDOW => {
                // Loop around to reevaluate the APIC.
                &mut self.backing.vtls[intercepted_vtl]
                    .exit_stats
                    .interrupt_window
            }
            VmxExitBasic::NMI_WINDOW => {
                // Loop around to reevaluate pending NMIs.
                &mut self.backing.vtls[intercepted_vtl].exit_stats.nmi_window
            }
            VmxExitBasic::HW_INTERRUPT => {
                if cfg!(feature = "gdb") {
                    // Check if the interrupt was triggered by a hardware breakpoint.
                    let debug_regs = self
                        .access_state(intercepted_vtl.into())
                        .debug_regs()
                        .expect("register query should not fail");
                    // The lowest four bits of DR6 indicate which of the
                    // four breakpoints triggered.
                    breakpoint_debug_exception = debug_regs.dr6.trailing_zeros() < 4;
                }
                &mut self.backing.vtls[intercepted_vtl].exit_stats.hw_interrupt
            }
            VmxExitBasic::SMI_INTR => &mut self.backing.vtls[intercepted_vtl].exit_stats.smi_intr,
            VmxExitBasic::PAUSE_INSTRUCTION => {
                &mut self.backing.vtls[intercepted_vtl].exit_stats.pause
            }
            VmxExitBasic::TDCALL => {
                // If the proxy synic is local, then the host did not get this
                // instruction, and we need to handle it.
                if self.backing.untrusted_synic.is_some() {
                    assert_eq!(intercepted_vtl, GuestVtl::Vtl0);
                    self.handle_tdvmcall(dev, intercepted_vtl);
                } else if self.cvm_partition().hide_isolation {
                    // TDCALL is not valid when hiding isolation. Inject a #UD.
                    self.backing.vtls[intercepted_vtl].interruption_information =
                        InterruptionInformation::new()
                            .with_valid(true)
                            .with_vector(x86defs::Exception::INVALID_OPCODE.0)
                            .with_interruption_type(INTERRUPT_TYPE_HARDWARE_EXCEPTION);
                }
                &mut self.backing.vtls[intercepted_vtl].exit_stats.tdcall
            }
            VmxExitBasic::EXCEPTION => {
                tracing::trace!(
                    "Caught Exception: {:?}",
                    exit_info._exit_interruption_info()
                );
                if cfg!(feature = "gdb") {
                    breakpoint_debug_exception = true;
                }
                &mut self.backing.vtls[intercepted_vtl].exit_stats.exception
            }
            VmxExitBasic::TRIPLE_FAULT => {
                return Err(VpHaltReason::TripleFault {
                    vtl: intercepted_vtl.into(),
                });
            }
            VmxExitBasic::GDTR_OR_IDTR => {
                let info = GdtrOrIdtrInstructionInfo::from(exit_info.instr_info().info());
                tracing::trace!("Intercepted GDT or IDT instruction: {:?}", info);
                let reg = match info.instruction() {
                    GdtrOrIdtrInstruction::Sidt | GdtrOrIdtrInstruction::Lidt => {
                        HvX64RegisterName::Idtr
                    }
                    GdtrOrIdtrInstruction::Sgdt | GdtrOrIdtrInstruction::Lgdt => {
                        HvX64RegisterName::Gdtr
                    }
                };
                // We only support fowarding intercepts for descriptor table loads today.
                if (info.instruction().is_load()
                    && !self.cvm_try_protect_secure_register_write(intercepted_vtl, reg, 0))
                    || !info.instruction().is_load()
                {
                    self.emulate_gdtr_or_idtr(intercepted_vtl, dev).await?;
                }
                &mut self.backing.vtls[intercepted_vtl]
                    .exit_stats
                    .descriptor_table
            }
            VmxExitBasic::LDTR_OR_TR => {
                let info = LdtrOrTrInstructionInfo::from(exit_info.instr_info().info());
                tracing::trace!("Intercepted LDT or TR instruction: {:?}", info);
                let reg = match info.instruction() {
                    LdtrOrTrInstruction::Sldt | LdtrOrTrInstruction::Lldt => {
                        HvX64RegisterName::Ldtr
                    }
                    LdtrOrTrInstruction::Str | LdtrOrTrInstruction::Ltr => HvX64RegisterName::Tr,
                };
                // We only support fowarding intercepts for descriptor table loads today.
                if (info.instruction().is_load()
                    && !self.cvm_try_protect_secure_register_write(intercepted_vtl, reg, 0))
                    || !info.instruction().is_load()
                {
                    self.emulate_ldtr_or_tr(intercepted_vtl, dev).await?;
                }
                &mut self.backing.vtls[intercepted_vtl]
                    .exit_stats
                    .descriptor_table
            }
            VmxExitBasic::TIMER_EXPIRED => {
                // Loop around to reevaluate pending interrupts.
                &mut self.backing.vtls[intercepted_vtl].exit_stats.timer_expired
            }
            _ => {
                return Err(dev.fatal_error(UnknownVmxExit(exit_info.code().vmx_exit()).into()));
            }
        };
        stat.increment();

        // Breakpoint exceptions may return a non-fatal error.
        // We dispatch here to correctly increment the counter.
        if cfg!(feature = "gdb") && breakpoint_debug_exception {
            self.handle_debug_exception(dev, intercepted_vtl)?;
        }

        Ok(())
    }

    /// Trace processor state for debugging purposes.
    fn trace_processor_state(&self, vtl: GuestVtl) {
        let raw_exit = self.runner.tdx_vp_enter_exit_info();
        tracing::error!(CVM_CONFIDENTIAL, ?raw_exit, "raw tdx vp enter exit info");

        let gprs = self.runner.tdx_enter_guest_gps();
        tracing::error!(CVM_CONFIDENTIAL, ?gprs, "guest gpr list");

        let TdxPrivateRegs {
            rflags,
            rip,
            rsp,
            ssp,
            rvi,
            svi,
            msr_kernel_gs_base,
            msr_star,
            msr_lstar,
            msr_sfmask,
            msr_xss,
            msr_tsc_aux,
            vp_entry_flags,
        } = self.backing.vtls[vtl].private_regs;
        tracing::error!(
            CVM_CONFIDENTIAL,
            rflags,
            rip,
            rsp,
            ssp,
            rvi,
            svi,
            msr_kernel_gs_base,
            msr_star,
            msr_lstar,
            msr_sfmask,
            msr_xss,
            msr_tsc_aux,
            ?vp_entry_flags,
            "private registers"
        );

        let physical_cr0 = self.runner.read_vmcs64(vtl, VmcsField::VMX_VMCS_GUEST_CR0);
        let shadow_cr0 = self
            .runner
            .read_vmcs64(vtl, VmcsField::VMX_VMCS_CR0_READ_SHADOW);
        let cr0_guest_host_mask: u64 = self
            .runner
            .read_vmcs64(vtl, VmcsField::VMX_VMCS_CR0_GUEST_HOST_MASK);
        tracing::error!(
            CVM_CONFIDENTIAL,
            physical_cr0,
            shadow_cr0,
            cr0_guest_host_mask,
            "cr0 values"
        );

        let physical_cr4 = self.runner.read_vmcs64(vtl, VmcsField::VMX_VMCS_GUEST_CR4);
        let shadow_cr4 = self
            .runner
            .read_vmcs64(vtl, VmcsField::VMX_VMCS_CR4_READ_SHADOW);
        let cr4_guest_host_mask = self
            .runner
            .read_vmcs64(vtl, VmcsField::VMX_VMCS_CR4_GUEST_HOST_MASK);
        tracing::error!(
            CVM_CONFIDENTIAL,
            physical_cr4,
            shadow_cr4,
            cr4_guest_host_mask,
            "cr4 values"
        );

        let cr3 = self.runner.read_vmcs64(vtl, VmcsField::VMX_VMCS_GUEST_CR3);
        tracing::error!(CVM_CONFIDENTIAL, cr3, "cr3");

        let cached_efer = self.backing.vtls[vtl].efer;
        let vmcs_efer = self.runner.read_vmcs64(vtl, VmcsField::VMX_VMCS_GUEST_EFER);
        let entry_controls = self
            .runner
            .read_vmcs32(vtl, VmcsField::VMX_VMCS_ENTRY_CONTROLS);
        tracing::error!(CVM_CONFIDENTIAL, cached_efer, vmcs_efer, "efer");
        tracing::error!(CVM_CONFIDENTIAL, entry_controls, "entry controls");

        let cs = self.read_segment(vtl, TdxSegmentReg::Cs);
        let ds = self.read_segment(vtl, TdxSegmentReg::Ds);
        let es = self.read_segment(vtl, TdxSegmentReg::Es);
        let fs = self.read_segment(vtl, TdxSegmentReg::Fs);
        let gs = self.read_segment(vtl, TdxSegmentReg::Gs);
        let ss = self.read_segment(vtl, TdxSegmentReg::Ss);
        let tr = self.read_segment(vtl, TdxSegmentReg::Tr);
        let ldtr = self.read_segment(vtl, TdxSegmentReg::Ldtr);

        tracing::error!(
            CVM_CONFIDENTIAL,
            ?cs,
            ?ds,
            ?es,
            ?fs,
            ?gs,
            ?ss,
            ?tr,
            ?ldtr,
            "segment values"
        );

        let exception_bitmap = self
            .runner
            .read_vmcs32(vtl, VmcsField::VMX_VMCS_EXCEPTION_BITMAP);
        tracing::error!(CVM_CONFIDENTIAL, exception_bitmap, "exception bitmap");

        let cached_processor_controls = self.backing.vtls[vtl].processor_controls;
        let vmcs_processor_controls = ProcessorControls::from(
            self.runner
                .read_vmcs32(vtl, VmcsField::VMX_VMCS_PROCESSOR_CONTROLS),
        );
        let vmcs_secondary_processor_controls = SecondaryProcessorControls::from(
            self.runner
                .read_vmcs32(vtl, VmcsField::VMX_VMCS_SECONDARY_PROCESSOR_CONTROLS),
        );
        tracing::error!(
            CVM_CONFIDENTIAL,
            ?cached_processor_controls,
            ?vmcs_processor_controls,
            ?vmcs_secondary_processor_controls,
            "processor controls"
        );

        if cached_processor_controls != vmcs_processor_controls {
            tracing::error!(CVM_ALLOWED, "BUGBUG: processor controls mismatch");
        }

        let cached_tpr_threshold = self.backing.vtls[vtl].tpr_threshold;
        let vmcs_tpr_threshold = self
            .runner
            .read_vmcs32(vtl, VmcsField::VMX_VMCS_TPR_THRESHOLD);
        tracing::error!(
            CVM_CONFIDENTIAL,
            cached_tpr_threshold,
            vmcs_tpr_threshold,
            "tpr threshold"
        );

        let cached_eoi_exit_bitmap = self.backing.eoi_exit_bitmap;
        let vmcs_eoi_exit_bitmap = {
            let fields = [
                VmcsField::VMX_VMCS_EOI_EXIT_0,
                VmcsField::VMX_VMCS_EOI_EXIT_1,
                VmcsField::VMX_VMCS_EOI_EXIT_2,
                VmcsField::VMX_VMCS_EOI_EXIT_3,
            ];
            fields
                .iter()
                .map(|field| self.runner.read_vmcs64(vtl, *field))
                .collect::<Vec<_>>()
        };
        tracing::error!(
            CVM_CONFIDENTIAL,
            ?cached_eoi_exit_bitmap,
            ?vmcs_eoi_exit_bitmap,
            "eoi exit bitmap"
        );

        let cached_interrupt_information = self.backing.vtls[vtl].interruption_information;
        let cached_interruption_set = self.backing.vtls[vtl].interruption_set;
        let vmcs_interrupt_information = self
            .runner
            .read_vmcs32(vtl, VmcsField::VMX_VMCS_ENTRY_INTERRUPT_INFO);
        let vmcs_entry_exception_code = self
            .runner
            .read_vmcs32(vtl, VmcsField::VMX_VMCS_ENTRY_EXCEPTION_ERROR_CODE);
        tracing::error!(
            CVM_CONFIDENTIAL,
            ?cached_interrupt_information,
            cached_interruption_set,
            vmcs_interrupt_information,
            vmcs_entry_exception_code,
            "interrupt information"
        );

        let guest_interruptibility = self
            .runner
            .read_vmcs32(vtl, VmcsField::VMX_VMCS_GUEST_INTERRUPTIBILITY);
        tracing::error!(
            CVM_CONFIDENTIAL,
            guest_interruptibility,
            "guest interruptibility"
        );

        let vmcs_sysenter_cs = self
            .runner
            .read_vmcs32(vtl, VmcsField::VMX_VMCS_GUEST_SYSENTER_CS_MSR);
        let vmcs_sysenter_esp = self
            .runner
            .read_vmcs64(vtl, VmcsField::VMX_VMCS_GUEST_SYSENTER_ESP_MSR);
        let vmcs_sysenter_eip = self
            .runner
            .read_vmcs64(vtl, VmcsField::VMX_VMCS_GUEST_SYSENTER_EIP_MSR);
        tracing::error!(
            CVM_CONFIDENTIAL,
            vmcs_sysenter_cs,
            vmcs_sysenter_esp,
            vmcs_sysenter_eip,
            "sysenter values"
        );

        let vmcs_pat = self.runner.read_vmcs64(vtl, VmcsField::VMX_VMCS_GUEST_PAT);
        tracing::error!(CVM_CONFIDENTIAL, vmcs_pat, "guest PAT");
    }

    fn handle_vm_enter_failed(
        &self,
        dev: &impl CpuIo,
        vtl: GuestVtl,
        vmx_exit: VmxExit,
    ) -> VpHaltReason {
        assert!(vmx_exit.vm_enter_failed());
        match vmx_exit.basic_reason() {
            VmxExitBasic::BAD_GUEST_STATE => {
                // Log system register state for debugging why we were
                // unable to enter the guest. This is a VMM bug.
                tracing::error!(CVM_ALLOWED, "VP.ENTER failed with bad guest state");
                self.trace_processor_state(vtl);

                dev.fatal_error(VmxBadGuestState.into())
            }
            _ => dev.fatal_error(UnknownVmxExit(vmx_exit).into()),
        }
    }

    fn advance_to_next_instruction(&mut self, vtl: GuestVtl) {
        let instr_info = TdxExit(self.runner.tdx_vp_enter_exit_info()).instr_info();
        let rip = &mut self.backing.vtls[vtl].private_regs.rip;
        *rip = rip.wrapping_add(instr_info.length().into());
    }

    fn clear_interrupt_shadow(&mut self, vtl: GuestVtl) {
        let mask = Interruptibility::new().with_blocked_by_sti(true);
        let value = Interruptibility::new().with_blocked_by_sti(false);
        self.runner.write_vmcs32(
            vtl,
            VmcsField::VMX_VMCS_GUEST_INTERRUPTIBILITY,
            mask.into(),
            value.into(),
        );
    }

    fn inject_gpf(&mut self, vtl: GuestVtl) {
        self.backing.vtls[vtl].interruption_information = InterruptionInformation::new()
            .with_valid(true)
            .with_vector(x86defs::Exception::GENERAL_PROTECTION_FAULT.0)
            .with_interruption_type(INTERRUPT_TYPE_HARDWARE_EXCEPTION)
            .with_deliver_error_code(true);
        self.backing.vtls[vtl].exception_error_code = 0;
    }

    fn handle_tdvmcall(&mut self, dev: &impl CpuIo, intercepted_vtl: GuestVtl) {
        let regs = self.runner.tdx_enter_guest_gps();
        if regs[TdxGp::R10] == 0 {
            // Architectural VMCALL.
            let result = match VmxExitBasic(regs[TdxGp::R11] as u16) {
                VmxExitBasic::MSR_WRITE => {
                    let msr = regs[TdxGp::R12] as u32;
                    let value = regs[TdxGp::R13];
                    match self.write_tdvmcall_msr(msr, value, intercepted_vtl) {
                        Ok(()) => {
                            tracing::debug!(msr, value, "tdvmcall msr write");
                            TdVmCallR10Result::SUCCESS
                        }
                        Err(err) => {
                            tracelimit::warn_ratelimited!(
                                CVM_ALLOWED,
                                msr,
                                ?err,
                                "failed tdvmcall msr write"
                            );
                            tracelimit::warn_ratelimited!(
                                CVM_CONFIDENTIAL,
                                value,
                                "failed tdvmcall msr write"
                            );
                            TdVmCallR10Result::OPERAND_INVALID
                        }
                    }
                }
                VmxExitBasic::MSR_READ => {
                    let msr = regs[TdxGp::R12] as u32;
                    match self.read_tdvmcall_msr(msr, intercepted_vtl) {
                        Ok(value) => {
                            tracing::debug!(msr, value, "tdvmcall msr read");
                            self.runner.tdx_enter_guest_gps_mut()[TdxGp::R11] = value;
                            TdVmCallR10Result::SUCCESS
                        }
                        Err(err) => {
                            tracelimit::warn_ratelimited!(
                                CVM_ALLOWED,
                                msr,
                                ?err,
                                "failed tdvmcall msr read"
                            );
                            TdVmCallR10Result::OPERAND_INVALID
                        }
                    }
                }
                subfunction => {
                    tracelimit::warn_ratelimited!(
                        CVM_ALLOWED,
                        ?subfunction,
                        "architectural vmcall not supported"
                    );
                    TdVmCallR10Result::OPERAND_INVALID
                }
            };
            self.runner.tdx_enter_guest_gps_mut()[TdxGp::R10] = result.0;
            self.backing.vtls[intercepted_vtl].private_regs.rip = self.backing.vtls
                [intercepted_vtl]
                .private_regs
                .rip
                .wrapping_add(4);
        } else {
            // This hypercall is normally handled by the hypervisor, so the gpas
            // given by the guest should all be shared. The hypervisor allows
            // gpas to be set with or without the shared gpa boundary bit, which
            // untrusted_dma_memory correctly models. Note that some Linux
            // guests will issue hypercalls without the boundary bit set,
            // whereas UEFI will issue with the bit set.
            let guest_memory = &self.shared.cvm.shared_memory;
            let handler = UhHypercallHandler {
                vp: &mut *self,
                bus: dev,
                trusted: false,
                intercepted_vtl,
            };

            UhHypercallHandler::TDCALL_DISPATCHER.dispatch(guest_memory, TdHypercall(handler));
        }
    }

    fn read_tdvmcall_msr(&mut self, msr: u32, intercepted_vtl: GuestVtl) -> Result<u64, MsrError> {
        match msr {
            msr @ (hvdef::HV_X64_MSR_GUEST_OS_ID | hvdef::HV_X64_MSR_VP_INDEX) => {
                self.backing.cvm.hv[intercepted_vtl].msr_read(msr)
            }
            _ => self
                .backing
                .untrusted_synic
                .as_mut()
                .unwrap()
                .read_nontimer_msr(msr),
        }
    }

    fn write_tdvmcall_msr(
        &mut self,
        msr: u32,
        value: u64,
        intercepted_vtl: GuestVtl,
    ) -> Result<(), MsrError> {
        match msr {
            hvdef::HV_X64_MSR_GUEST_OS_ID => {
                self.backing.cvm.hv[intercepted_vtl].msr_write_guest_os_id(value)
            }
            _ => {
                // If we get here we must have an untrusted synic, as otherwise
                // we wouldn't be handling the TDVMCALL that ends up here. Therefore
                // this is fine to unwrap.
                self.backing
                    .untrusted_synic
                    .as_mut()
                    .unwrap()
                    .write_nontimer_msr(
                        msr,
                        value,
                        &mut UntrustedSynicVtlProts(&self.partition.gm[GuestVtl::Vtl0]),
                    )?;
                // Propagate sint MSR writes to the hypervisor as well
                // so that the hypervisor can directly inject events.
                if matches!(msr, hvdef::HV_X64_MSR_SINT0..=hvdef::HV_X64_MSR_SINT15) {
                    if let Err(err) = self.runner.set_vp_register(
                        intercepted_vtl,
                        HvX64RegisterName(
                            HvX64RegisterName::Sint0.0 + (msr - hvdef::HV_X64_MSR_SINT0),
                        ),
                        value.into(),
                    ) {
                        tracelimit::warn_ratelimited!(
                            CVM_ALLOWED,
                            error = &err as &dyn std::error::Error,
                            "failed to set sint register"
                        );
                    }
                }
            }
        }

        Ok(())
    }

    fn read_msr_tdx(&mut self, msr: u32, vtl: GuestVtl) -> Result<u64, MsrError> {
        // TODO TDX: port remaining tdx and common values
        //
        // TODO TDX: consider if this can be shared with SnpBacked's
        // implementation. For the most part other than Intel/TDX specific
        // registers, MSR handling should be the same.

        match msr {
            // TODO TDX: LIFTED FROM WHP
            x86defs::X86X_IA32_MSR_PLATFORM_ID => {
                // Windows requires accessing this to boot. WHP
                // used to pass this through to the hardware,
                // but this regressed. Zero seems to work fine
                // for Windows.
                //
                // TODO: Pass through the host value if it can
                //       be retrieved.
                Ok(0)
            }

            x86defs::X86X_MSR_MTRR_CAP => {
                // Advertise the absence of MTRR capabilities, but include the availability of write
                // combining.
                Ok(0x400)
            }
            x86defs::X86X_MSR_MTRR_DEF_TYPE => {
                // Because the MTRR registers are advertised via CPUID, even though no actual ranges
                // are supported a guest may choose to write to this MSR. Implement it as read as
                // zero/write ignore.
                Ok(0)
            }
            x86defs::X86X_MSR_CSTAR => Ok(self.backing.vtls[vtl].msr_cstar),
            x86defs::X86X_MSR_MCG_CAP => Ok(0),
            x86defs::X86X_MSR_MCG_STATUS => Ok(0),
            x86defs::X86X_MSR_MC_UPDATE_PATCH_LEVEL => Ok(0xFFFFFFFF),
            x86defs::X86X_MSR_XSS => Ok(self.backing.vtls[vtl].private_regs.msr_xss),
            x86defs::X86X_IA32_MSR_MISC_ENABLE => Ok(hv1_emulator::x86::MISC_ENABLE.into()),
            x86defs::X86X_IA32_MSR_FEATURE_CONTROL => Ok(VMX_FEATURE_CONTROL_LOCKED),
            x86defs::X86X_MSR_CR_PAT => {
                let pat = self.runner.read_vmcs64(vtl, VmcsField::VMX_VMCS_GUEST_PAT);
                Ok(pat)
            }

            // Following MSRs are unconditionally read by Linux guests.
            // These are not virtualized and unsupported for L2-VMs
            x86defs::X86X_MSR_MISC_FEATURE_ENABLES
            | x86defs::X86X_MSR_PLATFORM_INFO
            | x86defs::X86X_MSR_PPIN_CTL
            | x86defs::X86X_IA32_MSR_SMI_COUNT
            | x86defs::X86X_MSR_UMWAIT_CONTROL
            | x86defs::X86X_AMD_MSR_DE_CFG
            | x86defs::X86X_IA32_MSR_RAPL_POWER_UNIT
            | x86defs::X86X_IA32_MSR_PKG_ENERGY_STATUS
            | x86defs::X86X_IA32_MSR_DRAM_ENERGY_STATUS
            | x86defs::X86X_IA32_MSR_PP0_ENERGY_STATUS => Ok(0),

            hvdef::HV_X64_MSR_GUEST_IDLE => {
                self.backing.cvm.lapics[vtl].activity = MpState::Idle;
                self.clear_interrupt_shadow(vtl);
                Ok(0)
            }
            X86X_MSR_EFER => Ok(self.backing.vtls[vtl].efer),

            _ => Err(MsrError::Unknown),
        }
    }

    fn write_msr_tdx(&mut self, msr: u32, value: u64, vtl: GuestVtl) -> Result<(), MsrError> {
        let state = &mut self.backing.vtls[vtl].private_regs;

        match msr {
            X86X_MSR_EFER => {
                self.write_efer(vtl, value)
                    .map_err(|_| MsrError::InvalidAccess)?;
                self.update_execution_mode(vtl);
            }
            x86defs::X86X_MSR_STAR => state.msr_star = value,
            x86defs::X86X_MSR_CSTAR => self.backing.vtls[vtl].msr_cstar = value,
            x86defs::X86X_MSR_LSTAR => state.msr_lstar = value,
            x86defs::X86X_MSR_SFMASK => state.msr_sfmask = value,
            x86defs::X86X_MSR_TSC_AUX => state.msr_tsc_aux = value,
            x86defs::X86X_MSR_SYSENTER_CS => {
                self.runner.write_vmcs32(
                    vtl,
                    VmcsField::VMX_VMCS_GUEST_SYSENTER_CS_MSR,
                    !0,
                    value as u32,
                );
            }
            x86defs::X86X_MSR_SYSENTER_EIP => {
                self.runner.write_vmcs64(
                    vtl,
                    VmcsField::VMX_VMCS_GUEST_SYSENTER_EIP_MSR,
                    !0,
                    value,
                );
            }
            x86defs::X86X_MSR_SYSENTER_ESP => {
                self.runner.write_vmcs64(
                    vtl,
                    VmcsField::VMX_VMCS_GUEST_SYSENTER_ESP_MSR,
                    !0,
                    value,
                );
            }
            x86defs::X86X_MSR_XSS => state.msr_xss = value,
            x86defs::X86X_MSR_MC_UPDATE_PATCH_LEVEL => {
                // Writing zero on intel platforms is allowed and ignored.
                if value != 0 {
                    return Err(MsrError::InvalidAccess);
                }
            }
            x86defs::X86X_IA32_MSR_MISC_ENABLE => {}
            x86defs::X86X_MSR_CR_PAT => {
                self.runner
                    .write_vmcs64(vtl, VmcsField::VMX_VMCS_GUEST_PAT, !0, value);
            }

            x86defs::X86X_MSR_MCG_STATUS => {
                // Writes are swallowed, except for reserved bits violations
                if x86defs::X86xMcgStatusRegister::from(value).reserved0() != 0 {
                    return Err(MsrError::InvalidAccess);
                }
            }

            // Ignore writes to this MSR
            x86defs::X86X_MSR_MTRR_DEF_TYPE => {}

            // Following MSRs are sometimes written by Windows guests.
            // These are not virtualized and unsupported for L2-VMs
            x86defs::X86X_MSR_BIOS_UPDT_TRIG => {}

            // Following MSRs are unconditionally written by Linux guests.
            // These are not virtualized and unsupported for L2-VMs
            x86defs::X86X_MSR_MISC_FEATURE_ENABLES
            | x86defs::X86X_MSR_PLATFORM_INFO
            | x86defs::X86X_MSR_PPIN_CTL
            | x86defs::X86X_IA32_MSR_SMI_COUNT
            | x86defs::X86X_MSR_UMWAIT_CONTROL
            | x86defs::X86X_AMD_MSR_DE_CFG
            | x86defs::X86X_IA32_MSR_RAPL_POWER_UNIT
            | x86defs::X86X_IA32_MSR_PKG_ENERGY_STATUS
            | x86defs::X86X_IA32_MSR_DRAM_ENERGY_STATUS
            | x86defs::X86X_IA32_MSR_PP0_ENERGY_STATUS => {}

            _ => return Err(MsrError::Unknown),
        }

        Ok(())
    }

    fn write_segment(
        &mut self,
        vtl: GuestVtl,
        seg: TdxSegmentReg,
        reg: SegmentRegister,
    ) -> Result<(), vp_state::Error> {
        // write base, selector, limit
        self.runner
            .write_vmcs16(vtl, seg.selector(), !0, reg.selector);
        self.runner.write_vmcs64(vtl, seg.base(), !0, reg.base);
        self.runner.write_vmcs32(vtl, seg.limit(), !0, reg.limit);

        // Mark segment not valid if its attributes indicate not present.
        let mut attributes = x86defs::vmx::VmxSegmentAttributes::from(reg.attributes as u32);
        attributes.set_null(!attributes.present());

        self.runner
            .write_vmcs32(vtl, seg.attributes(), !0, attributes.into());

        Ok(())
    }

    fn read_segment(&self, vtl: GuestVtl, seg: TdxSegmentReg) -> SegmentRegister {
        let selector = self.runner.read_vmcs16(vtl, seg.selector());
        let base = self.runner.read_vmcs64(vtl, seg.base());
        let limit = self.runner.read_vmcs32(vtl, seg.limit());
        let attributes = self.runner.read_vmcs32(vtl, seg.attributes());

        SegmentRegister {
            selector,
            base,
            limit,
            attributes: attributes as u16,
        }
    }

    fn long_mode(&self, vtl: GuestVtl) -> bool {
        let backing = &self.backing.vtls[vtl];
        backing.cr0.read(&self.runner) & X64_CR0_PE != 0 && backing.efer & X64_EFER_LMA != 0
    }
}

impl<T: CpuIo> X86EmulatorSupport for UhEmulationState<'_, '_, T, TdxBacked> {
    fn vp_index(&self) -> VpIndex {
        self.vp.vp_index()
    }

    fn flush(&mut self) {
        // no cached registers are modifiable by the emulator for TDX
    }

    fn vendor(&self) -> x86defs::cpuid::Vendor {
        self.vp.partition.caps.vendor
    }

    fn gp(&mut self, reg: Gp) -> u64 {
        self.vp.runner.tdx_enter_guest_gps()[reg as usize]
    }

    fn set_gp(&mut self, reg: Gp, v: u64) {
        self.vp.runner.tdx_enter_guest_gps_mut()[reg as usize] = v;
    }

    fn xmm(&mut self, index: usize) -> u128 {
        u128::from_ne_bytes(self.vp.runner.fx_state().xmm[index])
    }

    fn set_xmm(&mut self, index: usize, v: u128) {
        self.vp.runner.fx_state_mut().xmm[index] = v.to_ne_bytes();
    }

    fn rip(&mut self) -> u64 {
        self.vp.backing.vtls[self.vtl].private_regs.rip
    }

    fn set_rip(&mut self, v: u64) {
        self.vp.backing.vtls[self.vtl].private_regs.rip = v;
    }

    fn segment(&mut self, index: Segment) -> x86defs::SegmentRegister {
        let tdx_segment_index = match index {
            Segment::CS => TdxSegmentReg::Cs,
            Segment::ES => TdxSegmentReg::Es,
            Segment::SS => TdxSegmentReg::Ss,
            Segment::DS => TdxSegmentReg::Ds,
            Segment::FS => TdxSegmentReg::Fs,
            Segment::GS => TdxSegmentReg::Gs,
        };
        let reg = match tdx_segment_index {
            TdxSegmentReg::Cs => self.cache.segs[index as usize]
                .get_or_insert_with(|| TdxExit(self.vp.runner.tdx_vp_enter_exit_info()).cs()),
            _ => self.cache.segs[index as usize]
                .get_or_insert_with(|| self.vp.read_segment(self.vtl, tdx_segment_index)),
        };
        (*reg).into()
    }

    fn efer(&mut self) -> u64 {
        self.vp.backing.vtls[self.vtl].efer
    }

    fn cr0(&mut self) -> u64 {
        let reg = self
            .cache
            .cr0
            .get_or_insert_with(|| self.vp.backing.vtls[self.vtl].cr0.read(&self.vp.runner));
        *reg
    }

    fn rflags(&mut self) -> RFlags {
        self.vp.backing.vtls[self.vtl].private_regs.rflags.into()
    }

    fn set_rflags(&mut self, v: RFlags) {
        self.vp.backing.vtls[self.vtl].private_regs.rflags = v.into();
    }

    fn instruction_bytes(&self) -> &[u8] {
        &[]
    }

    fn physical_address(&self) -> Option<u64> {
        TdxExit(self.vp.runner.tdx_vp_enter_exit_info()).gpa()
    }

    fn initial_gva_translation(
        &mut self,
    ) -> Option<virt_support_x86emu::emulate::InitialTranslation> {
        let exit_info = TdxExit(self.vp.runner.tdx_vp_enter_exit_info());
        let ept_info = VmxEptExitQualification::from(exit_info.qualification());

        if exit_info.code().vmx_exit().basic_reason() == VmxExitBasic::EPT_VIOLATION
            && ept_info.gva_valid()
        {
            Some(virt_support_x86emu::emulate::InitialTranslation {
                gva: exit_info.gla().expect("already validated EPT exit"),
                gpa: exit_info.gpa().expect("already validated EPT exit"),
                translate_mode: match ept_info.access_mask() {
                    0x1 => TranslateMode::Read,
                    // As defined in "Table 28-7. Exit Qualification for EPT
                    // Violations" in the Intel SDM, the processor may set both
                    // the read and write bits in certain conditions:
                    //
                    // If accessed and dirty flags for EPT are enabled,
                    // processor accesses to guest paging-structure entries are
                    // treated as writes with regard to EPT violations (see
                    // Section 29.3.3.2). If such an access causes an EPT
                    // violation, the processor sets both bit 0 and bit 1 of the
                    // exit qualification.
                    //
                    // Treat both 0x2 and 0x3 as writes.
                    0x2 | 0x3 => TranslateMode::Write,
                    0x4 => TranslateMode::Execute,
                    _ => panic!("unexpected ept access mask 0x{:x}", ept_info.access_mask()),
                },
            })
        } else {
            None
        }
    }

    fn interruption_pending(&self) -> bool {
        self.interruption_pending
    }

    fn check_vtl_access(
        &mut self,
        _gpa: u64,
        _mode: TranslateMode,
    ) -> Result<(), virt_support_x86emu::emulate::EmuCheckVtlAccessError> {
        // Nothing to do here, the guest memory object will handle the check.
        Ok(())
    }

    fn translate_gva(
        &mut self,
        gva: u64,
        mode: TranslateMode,
    ) -> Result<
        virt_support_x86emu::emulate::EmuTranslateResult,
        virt_support_x86emu::emulate::EmuTranslateError,
    > {
        emulate_translate_gva(self, gva, mode)
    }

    fn inject_pending_event(&mut self, event_info: hvdef::HvX64PendingEvent) {
        assert!(event_info.reg_0.event_pending());
        assert_eq!(
            event_info.reg_0.event_type(),
            hvdef::HV_X64_PENDING_EVENT_EXCEPTION
        );
        assert!(!self.interruption_pending);

        // There's no interruption pending, so just inject the exception
        // directly without checking for double fault.
        TdxBacked::set_pending_exception(
            self.vp,
            self.vtl,
            HvX64PendingExceptionEvent::from(event_info.reg_0.into_bits()),
        );
    }

    fn is_gpa_mapped(&self, gpa: u64, write: bool) -> bool {
        // Ignore the VTOM address bit when checking, since memory is mirrored
        // across the VTOM.
        let vtom = self.vp.partition.caps.vtom.unwrap_or(0);
        debug_assert!(vtom == 0 || vtom.is_power_of_two());
        self.vp.partition.is_gpa_mapped(gpa & !vtom, write)
    }

    fn lapic_base_address(&self) -> Option<u64> {
        self.vp.backing.cvm.lapics[self.vtl].lapic.base_address()
    }

    fn lapic_read(&mut self, address: u64, data: &mut [u8]) {
        self.vp.backing.cvm.lapics[self.vtl]
            .lapic
            .access(&mut TdxApicClient {
                partition: self.vp.partition,
                dev: self.devices,
                vmtime: &self.vp.vmtime,
                apic_page: self.vp.runner.tdx_apic_page_mut(self.vtl),
                vtl: self.vtl,
            })
            .mmio_read(address, data);
    }

    fn lapic_write(&mut self, address: u64, data: &[u8]) {
        self.vp.backing.cvm.lapics[self.vtl]
            .lapic
            .access(&mut TdxApicClient {
                partition: self.vp.partition,
                dev: self.devices,
                vmtime: &self.vp.vmtime,
                apic_page: self.vp.runner.tdx_apic_page_mut(self.vtl),
                vtl: self.vtl,
            })
            .mmio_write(address, data);
    }

    fn monitor_support(&self) -> Option<&dyn EmulatorMonitorSupport> {
        Some(self)
    }
}

#[derive(Debug)]
enum TdxSegmentReg {
    Es,
    Cs,
    Ss,
    Ds,
    Fs,
    Gs,
    Ldtr,
    Tr,
}

impl TdxSegmentReg {
    /// The selector vmcs field code.
    fn selector(&self) -> VmcsField {
        match self {
            Self::Es => VmcsField::VMX_VMCS_GUEST_ES_SELECTOR,
            Self::Cs => VmcsField::VMX_VMCS_GUEST_CS_SELECTOR,
            Self::Ss => VmcsField::VMX_VMCS_GUEST_SS_SELECTOR,
            Self::Ds => VmcsField::VMX_VMCS_GUEST_DS_SELECTOR,
            Self::Fs => VmcsField::VMX_VMCS_GUEST_FS_SELECTOR,
            Self::Gs => VmcsField::VMX_VMCS_GUEST_GS_SELECTOR,
            Self::Ldtr => VmcsField::VMX_VMCS_GUEST_LDTR_SELECTOR,
            Self::Tr => VmcsField::VMX_VMCS_GUEST_TR_SELECTOR,
        }
    }

    /// The base vmcs field code.
    fn base(&self) -> VmcsField {
        match self {
            Self::Es => VmcsField::VMX_VMCS_GUEST_ES_BASE,
            Self::Cs => VmcsField::VMX_VMCS_GUEST_CS_BASE,
            Self::Ss => VmcsField::VMX_VMCS_GUEST_SS_BASE,
            Self::Ds => VmcsField::VMX_VMCS_GUEST_DS_BASE,
            Self::Fs => VmcsField::VMX_VMCS_GUEST_FS_BASE,
            Self::Gs => VmcsField::VMX_VMCS_GUEST_GS_BASE,
            Self::Ldtr => VmcsField::VMX_VMCS_GUEST_LDTR_BASE,
            Self::Tr => VmcsField::VMX_VMCS_GUEST_TR_BASE,
        }
    }

    /// The limit vmcs field code.
    fn limit(&self) -> VmcsField {
        match self {
            Self::Es => VmcsField::VMX_VMCS_GUEST_ES_LIMIT,
            Self::Cs => VmcsField::VMX_VMCS_GUEST_CS_LIMIT,
            Self::Ss => VmcsField::VMX_VMCS_GUEST_SS_LIMIT,
            Self::Ds => VmcsField::VMX_VMCS_GUEST_DS_LIMIT,
            Self::Fs => VmcsField::VMX_VMCS_GUEST_FS_LIMIT,
            Self::Gs => VmcsField::VMX_VMCS_GUEST_GS_LIMIT,
            Self::Ldtr => VmcsField::VMX_VMCS_GUEST_LDTR_LIMIT,
            Self::Tr => VmcsField::VMX_VMCS_GUEST_TR_LIMIT,
        }
    }

    // The attributes vmcs field code.
    fn attributes(&self) -> VmcsField {
        match self {
            Self::Es => VmcsField::VMX_VMCS_GUEST_ES_AR,
            Self::Cs => VmcsField::VMX_VMCS_GUEST_CS_AR,
            Self::Ss => VmcsField::VMX_VMCS_GUEST_SS_AR,
            Self::Ds => VmcsField::VMX_VMCS_GUEST_DS_AR,
            Self::Fs => VmcsField::VMX_VMCS_GUEST_FS_AR,
            Self::Gs => VmcsField::VMX_VMCS_GUEST_GS_AR,
            Self::Ldtr => VmcsField::VMX_VMCS_GUEST_LDTR_AR,
            Self::Tr => VmcsField::VMX_VMCS_GUEST_TR_AR,
        }
    }
}

#[derive(Debug)]
enum TdxTableReg {
    Idtr,
    Gdtr,
}

impl TdxTableReg {
    fn base_code(&self) -> VmcsField {
        match self {
            Self::Idtr => VmcsField::VMX_VMCS_GUEST_IDTR_BASE,
            Self::Gdtr => VmcsField::VMX_VMCS_GUEST_GDTR_BASE,
        }
    }

    fn limit_code(&self) -> VmcsField {
        match self {
            Self::Idtr => VmcsField::VMX_VMCS_GUEST_IDTR_LIMIT,
            Self::Gdtr => VmcsField::VMX_VMCS_GUEST_GDTR_LIMIT,
        }
    }
}

impl UhProcessor<'_, TdxBacked> {
    /// Handle a write to EFER, which requires special handling on TDX due to
    /// required bits and state updates.
    ///
    /// Note that a caller must also call [`Self::update_execution_mode`] after
    /// updating EFER.
    fn write_efer(&mut self, vtl: GuestVtl, efer: u64) -> Result<(), vp_state::Error> {
        if efer & (X64_EFER_SVME | X64_EFER_FFXSR) != 0 {
            return Err(vp_state::Error::InvalidValue(
                efer,
                "EFER",
                "SVME or FFXSR set",
            ));
        }

        // EFER.NXE must be 1.
        if efer & X64_EFER_NXE == 0 {
            return Err(vp_state::Error::InvalidValue(efer, "EFER", "NXE not set"));
        }

        // Update the local value of EFER and the VMCS.
        if self.backing.vtls[vtl].efer != efer {
            self.backing.vtls[vtl].efer = efer;
            self.runner
                .write_vmcs64(vtl, VmcsField::VMX_VMCS_GUEST_EFER, !0, efer);
        }

        Ok(())
    }

    /// Read CR0 that includes guest shadowed bits. This is the value the guest
    /// sees.
    fn read_cr0(&self, vtl: GuestVtl) -> u64 {
        self.backing.vtls[vtl].cr0.read(&self.runner)
    }

    /// Write to the guest CR0.
    fn write_cr0(&mut self, vtl: GuestVtl, value: u64) -> Result<(), vp_state::Error> {
        self.backing.vtls[vtl]
            .cr0
            .write(value | X64_CR0_ET, &mut self.runner)
    }

    fn read_cr4(&self, vtl: GuestVtl) -> u64 {
        self.backing.vtls[vtl].cr4.read(&self.runner)
    }

    fn write_cr4(&mut self, vtl: GuestVtl, value: u64) -> Result<(), vp_state::Error> {
        self.backing.vtls[vtl].cr4.write(value, &mut self.runner)
    }

    fn write_table_register(&mut self, vtl: GuestVtl, table: TdxTableReg, reg: TableRegister) {
        self.runner
            .write_vmcs64(vtl, table.base_code(), !0, reg.base);
        self.runner
            .write_vmcs32(vtl, table.limit_code(), !0, reg.limit.into());
    }

    fn read_table_register(&self, vtl: GuestVtl, table: TdxTableReg) -> TableRegister {
        let base = self.runner.read_vmcs64(vtl, table.base_code());
        let limit = self.runner.read_vmcs32(vtl, table.limit_code());

        TableRegister {
            base,
            limit: limit as u16,
        }
    }

    /// Update execution mode when CR0 or EFER is changed.
    fn update_execution_mode(&mut self, vtl: GuestVtl) {
        let lme = self.backing.vtls[vtl].efer & X64_EFER_LME == X64_EFER_LME;
        let pg = self.read_cr0(vtl) & X64_CR0_PG == X64_CR0_PG;
        let efer_lma = self.backing.vtls[vtl].efer & X64_EFER_LMA == X64_EFER_LMA;
        let lma = lme && pg;

        if lma != efer_lma {
            // Flip only the LMA bit.
            let new_efer = self.backing.vtls[vtl].efer ^ X64_EFER_LMA;
            self.write_efer(vtl, new_efer)
                .expect("EFER was valid before, it should still be valid");
        }

        self.runner.write_vmcs32(
            vtl,
            VmcsField::VMX_VMCS_ENTRY_CONTROLS,
            VMX_ENTRY_CONTROL_LONG_MODE_GUEST,
            if lma {
                VMX_ENTRY_CONTROL_LONG_MODE_GUEST
            } else {
                0
            },
        );
    }

    async fn emulate_gdtr_or_idtr(
        &mut self,
        vtl: GuestVtl,
        dev: &impl CpuIo,
    ) -> Result<(), VpHaltReason> {
        let exit_info = TdxExit(self.runner.tdx_vp_enter_exit_info());
        assert_eq!(
            exit_info.code().vmx_exit().basic_reason(),
            VmxExitBasic::GDTR_OR_IDTR
        );
        let instr_info = GdtrOrIdtrInstructionInfo::from(exit_info.instr_info().info());

        // Check if load instructions are executed outside of kernel mode.
        // Check if store instructions are blocked by UMIP.
        if (instr_info.instruction().is_load() && exit_info.cpl() != 0)
            || (!instr_info.instruction().is_load()
                && exit_info.cpl() > 0
                && self.read_cr4(vtl) & X64_CR4_UMIP != 0)
        {
            self.inject_gpf(vtl);
            return Ok(());
        }

        let (gva, segment) = self.compute_gva_for_table_access_emulation(
            exit_info.qualification(),
            (!instr_info.base_register_invalid()).then_some(instr_info.base_register()),
            (!instr_info.index_register_invalid()).then_some(instr_info.index_register()),
            instr_info.scaling(),
            instr_info.address_size(),
            instr_info.segment_register(),
        );

        let gm = &self.partition.gm[vtl];
        let interruption_pending = self.backing.vtls[vtl].interruption_information.valid();
        let len = 2 + if self.long_mode(vtl) { 8 } else { 4 };
        let mut buf = [0u8; 10];

        match instr_info.instruction() {
            GdtrOrIdtrInstruction::Sidt | GdtrOrIdtrInstruction::Sgdt => {
                let table = self.read_table_register(
                    vtl,
                    if matches!(instr_info.instruction(), GdtrOrIdtrInstruction::Sidt) {
                        TdxTableReg::Idtr
                    } else {
                        TdxTableReg::Gdtr
                    },
                );
                buf[..2].copy_from_slice(&table.limit.to_le_bytes());
                buf[2..].copy_from_slice(&table.base.to_le_bytes());
                let mut emulation_state = UhEmulationState {
                    vp: &mut *self,
                    interruption_pending,
                    devices: dev,
                    vtl,
                    cache: TdxEmulationCache::default(),
                };
                emulate_insn_memory_op(
                    &mut emulation_state,
                    gm,
                    dev,
                    gva,
                    segment,
                    x86emu::AlignmentMode::Unaligned,
                    EmulatedMemoryOperation::Write(&buf[..len]),
                )
                .await?;
            }

            GdtrOrIdtrInstruction::Lgdt | GdtrOrIdtrInstruction::Lidt => {
                let mut emulation_state = UhEmulationState {
                    vp: &mut *self,
                    interruption_pending,
                    devices: dev,
                    vtl,
                    cache: TdxEmulationCache::default(),
                };
                emulate_insn_memory_op(
                    &mut emulation_state,
                    gm,
                    dev,
                    gva,
                    segment,
                    x86emu::AlignmentMode::Unaligned,
                    EmulatedMemoryOperation::Read(&mut buf[..len]),
                )
                .await?;
                let table = TableRegister {
                    limit: u16::from_le_bytes(buf[..2].try_into().unwrap()),
                    base: u64::from_le_bytes(buf[2..len].try_into().unwrap()),
                };
                self.write_table_register(
                    vtl,
                    if matches!(instr_info.instruction(), GdtrOrIdtrInstruction::Lidt) {
                        TdxTableReg::Idtr
                    } else {
                        TdxTableReg::Gdtr
                    },
                    table,
                );
            }
        }

        self.advance_to_next_instruction(vtl);
        Ok(())
    }

    async fn emulate_ldtr_or_tr(
        &mut self,
        vtl: GuestVtl,
        dev: &impl CpuIo,
    ) -> Result<(), VpHaltReason> {
        let exit_info = TdxExit(self.runner.tdx_vp_enter_exit_info());
        assert_eq!(
            exit_info.code().vmx_exit().basic_reason(),
            VmxExitBasic::LDTR_OR_TR
        );
        let instr_info = LdtrOrTrInstructionInfo::from(exit_info.instr_info().info());

        // Check if load instructions are executed outside of kernel mode.
        // Check if store instructions are blocked by UMIP.
        if (instr_info.instruction().is_load() && exit_info.cpl() != 0)
            || (!instr_info.instruction().is_load()
                && exit_info.cpl() > 0
                && self.read_cr4(vtl) & X64_CR4_UMIP != 0)
        {
            self.inject_gpf(vtl);
            return Ok(());
        }

        let gm = &self.partition.gm[vtl];
        let interruption_pending = self.backing.vtls[vtl].interruption_information.valid();

        match instr_info.instruction() {
            LdtrOrTrInstruction::Sldt | LdtrOrTrInstruction::Str => {
                let value = self.runner.read_vmcs16(
                    vtl,
                    if matches!(instr_info.instruction(), LdtrOrTrInstruction::Sldt) {
                        TdxSegmentReg::Ldtr
                    } else {
                        TdxSegmentReg::Tr
                    }
                    .selector(),
                );

                if instr_info.memory_or_register() {
                    let gps = self.runner.tdx_enter_guest_gps_mut();
                    gps[instr_info.register_1() as usize] = value.into();
                } else {
                    let (gva, segment) = self.compute_gva_for_table_access_emulation(
                        exit_info.qualification(),
                        (!instr_info.base_register_invalid()).then_some(instr_info.base_register()),
                        (!instr_info.index_register_invalid())
                            .then_some(instr_info.index_register()),
                        instr_info.scaling(),
                        instr_info.address_size(),
                        instr_info.segment_register(),
                    );
                    let mut emulation_state = UhEmulationState {
                        vp: &mut *self,
                        interruption_pending,
                        devices: dev,
                        vtl,
                        cache: TdxEmulationCache::default(),
                    };
                    emulate_insn_memory_op(
                        &mut emulation_state,
                        gm,
                        dev,
                        gva,
                        segment,
                        x86emu::AlignmentMode::Standard,
                        EmulatedMemoryOperation::Write(&value.to_le_bytes()),
                    )
                    .await?;
                }
            }

            LdtrOrTrInstruction::Lldt | LdtrOrTrInstruction::Ltr => {
                let value = if instr_info.memory_or_register() {
                    let gps = self.runner.tdx_enter_guest_gps();
                    gps[instr_info.register_1() as usize] as u16
                } else {
                    let (gva, segment) = self.compute_gva_for_table_access_emulation(
                        exit_info.qualification(),
                        (!instr_info.base_register_invalid()).then_some(instr_info.base_register()),
                        (!instr_info.index_register_invalid())
                            .then_some(instr_info.index_register()),
                        instr_info.scaling(),
                        instr_info.address_size(),
                        instr_info.segment_register(),
                    );
                    let mut emulation_state = UhEmulationState {
                        vp: &mut *self,
                        interruption_pending,
                        devices: dev,
                        vtl,
                        cache: TdxEmulationCache::default(),
                    };
                    let mut buf = [0u8; 2];
                    emulate_insn_memory_op(
                        &mut emulation_state,
                        gm,
                        dev,
                        gva,
                        segment,
                        x86emu::AlignmentMode::Standard,
                        EmulatedMemoryOperation::Read(&mut buf),
                    )
                    .await?;
                    u16::from_le_bytes(buf)
                };
                self.runner.write_vmcs16(
                    vtl,
                    if matches!(instr_info.instruction(), LdtrOrTrInstruction::Lldt) {
                        TdxSegmentReg::Ldtr
                    } else {
                        TdxSegmentReg::Tr
                    }
                    .selector(),
                    !0,
                    value,
                );
            }
        }

        self.advance_to_next_instruction(vtl);
        Ok(())
    }

    fn compute_gva_for_table_access_emulation(
        &self,
        qualification: u64,
        base_reg: Option<u8>,
        index_reg: Option<u8>,
        scaling: u8,
        address_size: u8,
        segment_register: u8,
    ) -> (u64, Segment) {
        let gps = self.runner.tdx_enter_guest_gps();

        // Displacement is stored in the qualification field for these instructions.
        let mut gva = qualification;
        if let Some(base_register) = base_reg {
            gva += gps[base_register as usize];
        }
        if let Some(index_register) = index_reg {
            gva += gps[index_register as usize] << scaling;
        }
        match address_size {
            // 16-bit address size
            0 => gva &= 0xFFFF,
            // 32-bit address size
            1 => gva &= 0xFFFFFFFF,
            // 64-bit address size
            2 => {}
            _ => unreachable!(),
        }

        let segment = match segment_register {
            0 => Segment::ES,
            1 => Segment::CS,
            2 => Segment::SS,
            3 => Segment::DS,
            4 => Segment::FS,
            5 => Segment::GS,
            _ => unreachable!(),
        };

        (gva, segment)
    }
}

struct TdxApicClient<'a, T> {
    partition: &'a UhPartitionInner,
    apic_page: &'a mut ApicPage,
    dev: &'a T,
    vmtime: &'a VmTimeAccess,
    vtl: GuestVtl,
}

impl<T: CpuIo> ApicClient for TdxApicClient<'_, T> {
    fn cr8(&mut self) -> u32 {
        self.apic_page.tpr.value >> 4
    }

    fn set_cr8(&mut self, value: u32) {
        self.apic_page.tpr.value = value << 4;
    }

    fn set_apic_base(&mut self, _value: u64) {
        // No-op--the APIC base is stored in the APIC itself.
    }

    fn wake(&mut self, vp_index: VpIndex) {
        self.partition.vps[vp_index.index() as usize].wake(self.vtl, WakeReason::INTCON);
    }

    fn eoi(&mut self, vector: u8) {
        self.dev.handle_eoi(vector.into())
    }

    fn now(&mut self) -> vmcore::vmtime::VmTime {
        self.vmtime.now()
    }

    fn pull_offload(&mut self) -> ([u32; 8], [u32; 8]) {
        pull_apic_offload(self.apic_page)
    }
}

fn pull_apic_offload(page: &mut ApicPage) -> ([u32; 8], [u32; 8]) {
    let mut irr = [0; 8];
    let mut isr = [0; 8];
    for (((irr, page_irr), isr), page_isr) in irr
        .iter_mut()
        .zip(page.irr.iter_mut())
        .zip(isr.iter_mut())
        .zip(page.isr.iter_mut())
    {
        *irr = std::mem::take(&mut page_irr.value);
        *isr = std::mem::take(&mut page_isr.value);
    }
    (irr, isr)
}

impl<T> hv1_hypercall::X64RegisterState for UhHypercallHandler<'_, '_, T, TdxBacked> {
    fn rip(&mut self) -> u64 {
        self.vp.backing.vtls[self.intercepted_vtl].private_regs.rip
    }

    fn set_rip(&mut self, rip: u64) {
        self.vp.backing.vtls[self.intercepted_vtl].private_regs.rip = rip;
    }

    fn gp(&mut self, n: hv1_hypercall::X64HypercallRegister) -> u64 {
        let gps = self.vp.runner.tdx_enter_guest_gps();
        match n {
            hv1_hypercall::X64HypercallRegister::Rax => gps[TdxGp::RAX],
            hv1_hypercall::X64HypercallRegister::Rcx => gps[TdxGp::RCX],
            hv1_hypercall::X64HypercallRegister::Rdx => gps[TdxGp::RDX],
            hv1_hypercall::X64HypercallRegister::Rbx => gps[TdxGp::RBX],
            hv1_hypercall::X64HypercallRegister::Rsi => gps[TdxGp::RSI],
            hv1_hypercall::X64HypercallRegister::Rdi => gps[TdxGp::RDI],
            hv1_hypercall::X64HypercallRegister::R8 => gps[TdxGp::R8],
        }
    }

    fn set_gp(&mut self, n: hv1_hypercall::X64HypercallRegister, value: u64) {
        let gps = self.vp.runner.tdx_enter_guest_gps_mut();
        match n {
            hv1_hypercall::X64HypercallRegister::Rax => gps[TdxGp::RAX] = value,
            hv1_hypercall::X64HypercallRegister::Rcx => gps[TdxGp::RCX] = value,
            hv1_hypercall::X64HypercallRegister::Rdx => gps[TdxGp::RDX] = value,
            hv1_hypercall::X64HypercallRegister::Rbx => gps[TdxGp::RBX] = value,
            hv1_hypercall::X64HypercallRegister::Rsi => gps[TdxGp::RSI] = value,
            hv1_hypercall::X64HypercallRegister::Rdi => gps[TdxGp::RDI] = value,
            hv1_hypercall::X64HypercallRegister::R8 => gps[TdxGp::R8] = value,
        }
    }

    // TODO: cleanup xmm to not use same as mshv
    fn xmm(&mut self, n: usize) -> u128 {
        u128::from_ne_bytes(self.vp.runner.fx_state().xmm[n])
    }

    fn set_xmm(&mut self, n: usize, value: u128) {
        self.vp.runner.fx_state_mut().xmm[n] = value.to_ne_bytes();
    }
}

impl<T: CpuIo> UhHypercallHandler<'_, '_, T, TdxBacked> {
    const TDX_DISPATCHER: hv1_hypercall::Dispatcher<Self> = hv1_hypercall::dispatcher!(
        Self,
        [
            hv1_hypercall::HvModifySparseGpaPageHostVisibility,
            hv1_hypercall::HvQuerySparseGpaPageHostVisibility,
            hv1_hypercall::HvX64StartVirtualProcessor,
            hv1_hypercall::HvGetVpIndexFromApicId,
            hv1_hypercall::HvRetargetDeviceInterrupt,
            hv1_hypercall::HvFlushVirtualAddressList,
            hv1_hypercall::HvFlushVirtualAddressListEx,
            hv1_hypercall::HvFlushVirtualAddressSpace,
            hv1_hypercall::HvFlushVirtualAddressSpaceEx,
            hv1_hypercall::HvPostMessage,
            hv1_hypercall::HvSignalEvent,
            hv1_hypercall::HvExtQueryCapabilities,
            hv1_hypercall::HvGetVpRegisters,
            hv1_hypercall::HvSetVpRegisters,
            hv1_hypercall::HvEnablePartitionVtl,
            hv1_hypercall::HvX64EnableVpVtl,
            hv1_hypercall::HvVtlCall,
            hv1_hypercall::HvVtlReturn,
            hv1_hypercall::HvModifyVtlProtectionMask,
            hv1_hypercall::HvX64TranslateVirtualAddress,
            hv1_hypercall::HvSendSyntheticClusterIpi,
            hv1_hypercall::HvSendSyntheticClusterIpiEx,
            hv1_hypercall::HvInstallIntercept,
            hv1_hypercall::HvAssertVirtualInterrupt,
        ]
    );

    /// Hypercalls that come through a tdg.vp.vmcall tdcall instruction.
    ///
    /// This is just to handle the proxy synic.
    const TDCALL_DISPATCHER: hv1_hypercall::Dispatcher<Self> = hv1_hypercall::dispatcher!(
        Self,
        [hv1_hypercall::HvPostMessage, hv1_hypercall::HvSignalEvent],
    );
}

impl AccessVpState for UhVpStateAccess<'_, '_, TdxBacked> {
    type Error = vp_state::Error;

    fn caps(&self) -> &virt::x86::X86PartitionCapabilities {
        &self.vp.partition.caps
    }

    fn commit(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn registers(&mut self) -> Result<Registers, Self::Error> {
        let gps = self.vp.runner.tdx_enter_guest_gps();

        let cs = self.vp.read_segment(self.vtl, TdxSegmentReg::Cs);
        let ds = self.vp.read_segment(self.vtl, TdxSegmentReg::Ds);
        let es = self.vp.read_segment(self.vtl, TdxSegmentReg::Es);
        let fs = self.vp.read_segment(self.vtl, TdxSegmentReg::Fs);
        let gs = self.vp.read_segment(self.vtl, TdxSegmentReg::Gs);
        let ss = self.vp.read_segment(self.vtl, TdxSegmentReg::Ss);
        let tr = self.vp.read_segment(self.vtl, TdxSegmentReg::Tr);
        let ldtr = self.vp.read_segment(self.vtl, TdxSegmentReg::Ldtr);

        let gdtr = self.vp.read_table_register(self.vtl, TdxTableReg::Gdtr);
        let idtr = self.vp.read_table_register(self.vtl, TdxTableReg::Idtr);

        let cr0 = self.vp.read_cr0(self.vtl);
        let cr2 = self.vp.runner.cr2();
        let cr3 = self
            .vp
            .runner
            .read_vmcs64(self.vtl, VmcsField::VMX_VMCS_GUEST_CR3);
        let cr4 = self.vp.read_cr4(self.vtl);

        let cr8 = self.vp.runner.tdx_apic_page(self.vtl).tpr.value >> 4;

        let efer = self.vp.backing.vtls[self.vtl].efer;

        Ok(Registers {
            rax: gps[TdxGp::RAX],
            rcx: gps[TdxGp::RCX],
            rdx: gps[TdxGp::RDX],
            rbx: gps[TdxGp::RBX],
            rsp: self.vp.backing.vtls[self.vtl].private_regs.rsp,
            rbp: gps[TdxGp::RBP],
            rsi: gps[TdxGp::RSI],
            rdi: gps[TdxGp::RDI],
            r8: gps[TdxGp::R8],
            r9: gps[TdxGp::R9],
            r10: gps[TdxGp::R10],
            r11: gps[TdxGp::R11],
            r12: gps[TdxGp::R12],
            r13: gps[TdxGp::R13],
            r14: gps[TdxGp::R14],
            r15: gps[TdxGp::R15],
            rip: self.vp.backing.vtls[self.vtl].private_regs.rip,
            rflags: self.vp.backing.vtls[self.vtl].private_regs.rflags,
            cs,
            ds,
            es,
            fs,
            gs,
            ss,
            tr,
            ldtr,
            gdtr,
            idtr,
            cr0,
            cr2,
            cr3,
            cr4,
            cr8: cr8.into(),
            efer,
        })
    }

    fn set_registers(&mut self, value: &Registers) -> Result<(), Self::Error> {
        let Registers {
            rax,
            rcx,
            rdx,
            rbx,
            rsp,
            rbp,
            rsi,
            rdi,
            r8,
            r9,
            r10,
            r11,
            r12,
            r13,
            r14,
            r15,
            rip,
            rflags,
            cs,
            ds,
            es,
            fs,
            gs,
            ss,
            tr,
            ldtr,
            gdtr,
            idtr,
            cr0,
            cr2,
            cr3,
            cr4,
            cr8,
            efer,
        } = value;

        let gps = self.vp.runner.tdx_enter_guest_gps_mut();
        gps[TdxGp::RAX] = *rax;
        gps[TdxGp::RCX] = *rcx;
        gps[TdxGp::RDX] = *rdx;
        gps[TdxGp::RBX] = *rbx;
        self.vp.backing.vtls[self.vtl].private_regs.rsp = *rsp;
        gps[TdxGp::RBP] = *rbp;
        gps[TdxGp::RSI] = *rsi;
        gps[TdxGp::RDI] = *rdi;
        gps[TdxGp::R8] = *r8;
        gps[TdxGp::R9] = *r9;
        gps[TdxGp::R10] = *r10;
        gps[TdxGp::R11] = *r11;
        gps[TdxGp::R12] = *r12;
        gps[TdxGp::R13] = *r13;
        gps[TdxGp::R14] = *r14;
        gps[TdxGp::R15] = *r15;
        self.vp.backing.vtls[self.vtl].private_regs.rip = *rip;
        // BUGBUG: rflags set also updates interrupts in hcl
        self.vp.backing.vtls[self.vtl].private_regs.rflags = *rflags;

        // Set segment registers
        self.vp.write_segment(self.vtl, TdxSegmentReg::Cs, *cs)?;
        self.vp.write_segment(self.vtl, TdxSegmentReg::Ds, *ds)?;
        self.vp.write_segment(self.vtl, TdxSegmentReg::Es, *es)?;
        self.vp.write_segment(self.vtl, TdxSegmentReg::Fs, *fs)?;
        self.vp.write_segment(self.vtl, TdxSegmentReg::Gs, *gs)?;
        self.vp.write_segment(self.vtl, TdxSegmentReg::Ss, *ss)?;
        self.vp.write_segment(self.vtl, TdxSegmentReg::Tr, *tr)?;
        self.vp
            .write_segment(self.vtl, TdxSegmentReg::Ldtr, *ldtr)?;

        // Set table registers
        self.vp
            .write_table_register(self.vtl, TdxTableReg::Gdtr, *gdtr);
        self.vp
            .write_table_register(self.vtl, TdxTableReg::Idtr, *idtr);

        self.vp.write_cr0(self.vtl, *cr0)?;

        // CR2 is shared with the kernel, so set it in the VP run page which
        // will be set before lower VTL entry.
        self.vp.runner.set_cr2(*cr2);

        self.vp
            .runner
            .write_vmcs64(self.vtl, VmcsField::VMX_VMCS_GUEST_CR3, !0, *cr3);

        self.vp.write_cr4(self.vtl, *cr4)?;

        self.vp.runner.tdx_apic_page_mut(self.vtl).tpr.value = (*cr8 << 4) as u32;

        self.vp.write_efer(self.vtl, *efer)?;

        // Execution mode must be updated after setting EFER and CR0.
        self.vp.update_execution_mode(self.vtl);

        Ok(())
    }

    fn activity(&mut self) -> Result<vp::Activity, Self::Error> {
        let lapic = &self.vp.backing.cvm.lapics[self.vtl];
        let interruptibility: Interruptibility = self
            .vp
            .runner
            .read_vmcs32(self.vtl, VmcsField::VMX_VMCS_GUEST_INTERRUPTIBILITY)
            .into();
        Ok(vp::Activity {
            mp_state: lapic.activity,
            nmi_pending: lapic.nmi_pending,
            nmi_masked: interruptibility.blocked_by_nmi(),
            interrupt_shadow: interruptibility.blocked_by_sti()
                || interruptibility.blocked_by_movss(),
            pending_event: None,        // TODO TDX
            pending_interruption: None, // TODO TDX
        })
    }

    fn set_activity(&mut self, value: &vp::Activity) -> Result<(), Self::Error> {
        let &vp::Activity {
            mp_state,
            nmi_pending,
            nmi_masked,
            interrupt_shadow,
            pending_event: _,        // TODO TDX
            pending_interruption: _, // TODO TDX
        } = value;
        self.vp.backing.cvm.lapics[self.vtl].activity = mp_state;
        self.vp.backing.cvm.lapics[self.vtl].nmi_pending = nmi_pending;
        let interruptibility = Interruptibility::new()
            .with_blocked_by_movss(interrupt_shadow)
            .with_blocked_by_nmi(nmi_masked);
        self.vp.runner.write_vmcs32(
            self.vtl,
            VmcsField::VMX_VMCS_GUEST_INTERRUPTIBILITY,
            !0,
            interruptibility.into(),
        );
        Ok(())
    }

    fn xsave(&mut self) -> Result<vp::Xsave, Self::Error> {
        // TODO: needed?
        Err(vp_state::Error::Unimplemented("xsave"))
    }

    fn set_xsave(&mut self, _value: &vp::Xsave) -> Result<(), Self::Error> {
        // TODO: needed?
        Err(vp_state::Error::Unimplemented("xsave"))
    }

    fn apic(&mut self) -> Result<vp::Apic, Self::Error> {
        self.vp.access_apic_without_offload(self.vtl, |vp| {
            Ok(vp.backing.cvm.lapics[self.vtl].lapic.save())
        })
    }

    fn set_apic(&mut self, value: &vp::Apic) -> Result<(), Self::Error> {
        self.vp.access_apic_without_offload(self.vtl, |vp| {
            vp.backing.cvm.lapics[self.vtl]
                .lapic
                .restore(value)
                .map_err(vp_state::Error::InvalidApicBase)?;

            Ok(())
        })
    }

    fn xcr(&mut self) -> Result<vp::Xcr0, Self::Error> {
        Ok(vp::Xcr0 {
            value: self
                .vp
                .runner
                .get_vp_register(self.vtl, HvX64RegisterName::Xfem)
                .unwrap()
                .as_u64(),
        })
    }

    fn set_xcr(&mut self, _value: &vp::Xcr0) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("xcr"))
    }

    fn xss(&mut self) -> Result<vp::Xss, Self::Error> {
        Ok(vp::Xss {
            value: self.vp.backing.vtls[self.vtl].private_regs.msr_xss,
        })
    }

    fn set_xss(&mut self, value: &vp::Xss) -> Result<(), Self::Error> {
        self.vp.backing.vtls[self.vtl].private_regs.msr_xss = value.value;
        Ok(())
    }

    fn mtrrs(&mut self) -> Result<vp::Mtrrs, Self::Error> {
        Ok(vp::Mtrrs {
            msr_mtrr_def_type: 0, // TODO TDX: MTRRs
            fixed: [0; 11],       // TODO TDX: MTRRs
            variable: [0; 16],    // TODO TDX: MTRRs
        })
    }

    fn set_mtrrs(&mut self, _value: &vp::Mtrrs) -> Result<(), Self::Error> {
        // TODO TDX: MTRRs
        Ok(())
    }

    fn pat(&mut self) -> Result<vp::Pat, Self::Error> {
        let msr_cr_pat = self
            .vp
            .runner
            .read_vmcs64(self.vtl, VmcsField::VMX_VMCS_GUEST_PAT);
        Ok(vp::Pat { value: msr_cr_pat })
    }

    fn set_pat(&mut self, value: &vp::Pat) -> Result<(), Self::Error> {
        self.vp
            .runner
            .write_vmcs64(self.vtl, VmcsField::VMX_VMCS_GUEST_PAT, !0, value.value);
        Ok(())
    }

    fn virtual_msrs(&mut self) -> Result<vp::VirtualMsrs, Self::Error> {
        let state = &self.vp.backing.vtls[self.vtl].private_regs;

        let sysenter_cs = self
            .vp
            .runner
            .read_vmcs32(self.vtl, VmcsField::VMX_VMCS_GUEST_SYSENTER_CS_MSR)
            .into();
        let sysenter_eip = self
            .vp
            .runner
            .read_vmcs64(self.vtl, VmcsField::VMX_VMCS_GUEST_SYSENTER_EIP_MSR);
        let sysenter_esp = self
            .vp
            .runner
            .read_vmcs64(self.vtl, VmcsField::VMX_VMCS_GUEST_SYSENTER_ESP_MSR);

        Ok(vp::VirtualMsrs {
            kernel_gs_base: state.msr_kernel_gs_base,
            sysenter_cs,
            sysenter_eip,
            sysenter_esp,
            star: state.msr_star,
            lstar: state.msr_lstar,
            cstar: self.vp.backing.vtls[self.vtl].msr_cstar,
            sfmask: state.msr_sfmask,
        })
    }

    fn set_virtual_msrs(&mut self, value: &vp::VirtualMsrs) -> Result<(), Self::Error> {
        let &vp::VirtualMsrs {
            kernel_gs_base,
            sysenter_cs,
            sysenter_eip,
            sysenter_esp,
            star,
            lstar,
            cstar,
            sfmask,
        } = value;

        let state = &mut self.vp.backing.vtls[self.vtl].private_regs;
        state.msr_kernel_gs_base = kernel_gs_base;
        state.msr_star = star;
        state.msr_lstar = lstar;
        state.msr_sfmask = sfmask;

        self.vp.runner.write_vmcs32(
            self.vtl,
            VmcsField::VMX_VMCS_GUEST_SYSENTER_CS_MSR,
            !0,
            sysenter_cs as u32,
        );
        self.vp.runner.write_vmcs64(
            self.vtl,
            VmcsField::VMX_VMCS_GUEST_SYSENTER_EIP_MSR,
            !0,
            sysenter_eip,
        );
        self.vp.runner.write_vmcs64(
            self.vtl,
            VmcsField::VMX_VMCS_GUEST_SYSENTER_ESP_MSR,
            !0,
            sysenter_esp,
        );

        self.vp.backing.vtls[self.vtl].msr_cstar = cstar;

        Ok(())
    }

    fn debug_regs(&mut self) -> Result<vp::DebugRegisters, Self::Error> {
        let mut values = [0u64.into(); 5];
        self.vp
            .runner
            .get_vp_registers(
                self.vtl,
                &[
                    HvX64RegisterName::Dr0,
                    HvX64RegisterName::Dr1,
                    HvX64RegisterName::Dr2,
                    HvX64RegisterName::Dr3,
                    HvX64RegisterName::Dr6,
                ],
                &mut values,
            )
            .map_err(vp_state::Error::GetRegisters)?;

        let dr7 = self
            .vp
            .runner
            .read_vmcs64(self.vtl, VmcsField::VMX_VMCS_GUEST_DR7);

        Ok(vp::DebugRegisters {
            dr0: values[0].as_u64(),
            dr1: values[1].as_u64(),
            dr2: values[2].as_u64(),
            dr3: values[3].as_u64(),
            dr6: values[4].as_u64(),
            dr7,
        })
    }

    fn set_debug_regs(&mut self, value: &vp::DebugRegisters) -> Result<(), Self::Error> {
        let &vp::DebugRegisters {
            dr0,
            dr1,
            dr2,
            dr3,
            dr6,
            dr7,
        } = value;
        self.vp
            .runner
            .set_vp_registers(
                self.vtl,
                [
                    (HvX64RegisterName::Dr0, dr0),
                    (HvX64RegisterName::Dr1, dr1),
                    (HvX64RegisterName::Dr2, dr2),
                    (HvX64RegisterName::Dr3, dr3),
                    (HvX64RegisterName::Dr6, dr6),
                ],
            )
            .map_err(vp_state::Error::SetRegisters)?;

        self.vp
            .runner
            .write_vmcs64(self.vtl, VmcsField::VMX_VMCS_GUEST_DR7, !0, dr7);

        Ok(())
    }

    fn tsc(&mut self) -> Result<vp::Tsc, Self::Error> {
        Err(vp_state::Error::Unimplemented("tsc"))
    }

    fn set_tsc(&mut self, _value: &vp::Tsc) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("tsc"))
    }

    fn tsc_aux(&mut self) -> Result<vp::TscAux, Self::Error> {
        Ok(vp::TscAux {
            value: self.vp.backing.vtls[self.vtl].private_regs.msr_tsc_aux,
        })
    }

    fn set_tsc_aux(&mut self, value: &vp::TscAux) -> Result<(), Self::Error> {
        self.vp.backing.vtls[self.vtl].private_regs.msr_tsc_aux = value.value;
        Ok(())
    }

    fn cet(&mut self) -> Result<vp::Cet, Self::Error> {
        Err(vp_state::Error::Unimplemented("cet"))
    }

    fn set_cet(&mut self, _value: &vp::Cet) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("cet"))
    }

    fn cet_ss(&mut self) -> Result<vp::CetSs, Self::Error> {
        Err(vp_state::Error::Unimplemented("cet_ss"))
    }

    fn set_cet_ss(&mut self, _value: &vp::CetSs) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("cet_ss"))
    }

    fn synic_msrs(&mut self) -> Result<vp::SyntheticMsrs, Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_msrs"))
    }

    fn set_synic_msrs(&mut self, _value: &vp::SyntheticMsrs) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_msrs"))
    }

    fn synic_message_page(&mut self) -> Result<vp::SynicMessagePage, Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_message_page"))
    }

    fn set_synic_message_page(&mut self, _value: &vp::SynicMessagePage) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_message_page"))
    }

    fn synic_event_flags_page(&mut self) -> Result<vp::SynicEventFlagsPage, Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_event_flags_page"))
    }

    fn set_synic_event_flags_page(
        &mut self,
        _value: &vp::SynicEventFlagsPage,
    ) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_event_flags_page"))
    }

    fn synic_message_queues(&mut self) -> Result<vp::SynicMessageQueues, Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_message_queues"))
    }

    fn set_synic_message_queues(
        &mut self,
        _value: &vp::SynicMessageQueues,
    ) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_message_queues"))
    }

    fn synic_timers(&mut self) -> Result<vp::SynicTimers, Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_timers"))
    }

    fn set_synic_timers(&mut self, _value: &vp::SynicTimers) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_timers"))
    }
}

/// Compute the index of the highest vector set in IRR/ISR, or 0
/// if no vector is set. (Vectors 0-15 are invalid so this is not
/// ambiguous.)
fn top_vector(reg: &[ApicRegister; 8]) -> u8 {
    reg.iter()
        .enumerate()
        .rev()
        .find_map(|(i, r)| {
            (r.value != 0).then(|| (i as u32 * 32 + (31 - r.value.leading_zeros())) as u8)
        })
        .unwrap_or(0)
}

struct TdHypercall<'a, 'b, T>(UhHypercallHandler<'a, 'b, T, TdxBacked>);

impl<'a, 'b, T> AsHandler<UhHypercallHandler<'a, 'b, T, TdxBacked>> for TdHypercall<'a, 'b, T> {
    fn as_handler(&mut self) -> &mut UhHypercallHandler<'a, 'b, T, TdxBacked> {
        &mut self.0
    }
}

impl<T> HypercallIo for TdHypercall<'_, '_, T> {
    fn advance_ip(&mut self) {
        self.0.vp.runner.tdx_enter_guest_gps_mut()[TdxGp::R10] = 0;
        self.0.vp.backing.vtls[self.0.intercepted_vtl]
            .private_regs
            .rip = self.0.vp.backing.vtls[self.0.intercepted_vtl]
            .private_regs
            .rip
            .wrapping_add(4);
    }

    fn retry(&mut self, control: u64) {
        self.0.vp.runner.tdx_enter_guest_gps_mut()[TdxGp::R10] = control;
        self.set_result(hvdef::hypercall::HypercallOutput::from(HvError::Timeout).into());
    }

    fn control(&mut self) -> u64 {
        self.0.vp.runner.tdx_enter_guest_gps()[TdxGp::R10]
    }

    fn input_gpa(&mut self) -> u64 {
        self.0.vp.runner.tdx_enter_guest_gps()[TdxGp::RDX]
    }

    fn output_gpa(&mut self) -> u64 {
        self.0.vp.runner.tdx_enter_guest_gps()[TdxGp::R8]
    }

    fn fast_register_pair_count(&mut self) -> usize {
        7
    }

    fn extended_fast_hypercalls_ok(&mut self) -> bool {
        false
    }

    fn fast_input(&mut self, buf: &mut [[u64; 2]], _output_register_pairs: usize) -> usize {
        self.fast_regs(0, buf);
        buf.len()
    }

    fn fast_output(&mut self, _starting_pair_index: usize, buf: &[[u64; 2]]) {
        assert!(buf.is_empty());
    }

    fn vtl_input(&mut self) -> u64 {
        unreachable!()
    }

    fn set_result(&mut self, n: u64) {
        self.0.vp.runner.tdx_enter_guest_gps_mut()[TdxGp::R11] = n;
    }

    fn fast_regs(&mut self, starting_pair_index: usize, buf: &mut [[u64; 2]]) {
        let regs = self.0.vp.runner.tdx_enter_guest_gps();
        let fx_state = self.0.vp.runner.fx_state();
        for (i, [low, high]) in buf.iter_mut().enumerate() {
            let index = i + starting_pair_index;
            if index == 0 {
                *low = regs[TdxGp::RDX];
                *high = regs[TdxGp::R8];
            } else {
                let value = u128::from_ne_bytes(fx_state.xmm[index - 1]);
                *low = value as u64;
                *high = (value >> 64) as u64;
            }
        }
    }
}

impl<T> hv1_hypercall::VtlSwitchOps for UhHypercallHandler<'_, '_, T, TdxBacked> {
    fn advance_ip(&mut self) {
        let long_mode = self.vp.long_mode(self.intercepted_vtl);
        let mut io = hv1_hypercall::X64RegisterIo::new(self, long_mode);
        io.advance_ip();
    }

    fn inject_invalid_opcode_fault(&mut self) {
        self.vp.backing.vtls[self.intercepted_vtl].interruption_information =
            InterruptionInformation::new()
                .with_valid(true)
                .with_interruption_type(INTERRUPT_TYPE_HARDWARE_EXCEPTION)
                .with_vector(x86defs::Exception::INVALID_OPCODE.0);
    }
}

impl<T: CpuIo> hv1_hypercall::FlushVirtualAddressList for UhHypercallHandler<'_, '_, T, TdxBacked> {
    fn flush_virtual_address_list(
        &mut self,
        processor_set: ProcessorSet<'_>,
        flags: HvFlushFlags,
        gva_ranges: &[HvGvaRange],
    ) -> HvRepResult {
        hv1_hypercall::FlushVirtualAddressListEx::flush_virtual_address_list_ex(
            self,
            processor_set,
            flags,
            gva_ranges,
        )
    }
}

impl<T: CpuIo> hv1_hypercall::FlushVirtualAddressListEx
    for UhHypercallHandler<'_, '_, T, TdxBacked>
{
    fn flush_virtual_address_list_ex(
        &mut self,
        processor_set: ProcessorSet<'_>,
        flags: HvFlushFlags,
        gva_ranges: &[HvGvaRange],
    ) -> HvRepResult {
        self.hcvm_validate_flush_inputs(processor_set, flags, true)
            .map_err(|e| (e, 0))?;

        let vtl = self.intercepted_vtl;
        let flush_state = &self.vp.shared.flush_state[vtl];

        // If we fail to add ranges to the list for any reason then promote this request to a flush entire.
        if let Err(()) = Self::add_ranges_to_tlb_flush_list(
            flush_state,
            gva_ranges,
            flags.use_extended_range_format(),
        ) {
            if flags.non_global_mappings_only() {
                flush_state
                    .flush_entire_non_global_counter
                    .fetch_add(1, Ordering::Relaxed);
            } else {
                flush_state
                    .flush_entire_counter
                    .fetch_add(1, Ordering::Relaxed);
            }
        }

        // Send flush IPIs to the specified VPs.
        TdxTlbLockFlushAccess {
            vp_index: Some(self.vp.vp_index()),
            partition: self.vp.partition,
            shared: self.vp.shared,
        }
        .wake_processors_for_tlb_flush(vtl, (!flags.all_processors()).then_some(processor_set));

        // Mark that this VP needs to wait for all TLB locks to be released before returning.
        self.vp.set_wait_for_tlb_locks(vtl);

        Ok(())
    }
}

impl<T: CpuIo> hv1_hypercall::FlushVirtualAddressSpace
    for UhHypercallHandler<'_, '_, T, TdxBacked>
{
    fn flush_virtual_address_space(
        &mut self,
        processor_set: ProcessorSet<'_>,
        flags: HvFlushFlags,
    ) -> hvdef::HvResult<()> {
        hv1_hypercall::FlushVirtualAddressSpaceEx::flush_virtual_address_space_ex(
            self,
            processor_set,
            flags,
        )
    }
}

impl<T: CpuIo> hv1_hypercall::FlushVirtualAddressSpaceEx
    for UhHypercallHandler<'_, '_, T, TdxBacked>
{
    fn flush_virtual_address_space_ex(
        &mut self,
        processor_set: ProcessorSet<'_>,
        flags: HvFlushFlags,
    ) -> hvdef::HvResult<()> {
        self.hcvm_validate_flush_inputs(processor_set, flags, false)?;
        let vtl = self.intercepted_vtl;

        let flush_state = &self.vp.shared.flush_state[vtl];

        // Set flush entire.
        if flags.non_global_mappings_only() {
            flush_state
                .flush_entire_non_global_counter
                .fetch_add(1, Ordering::Relaxed);
        } else {
            flush_state
                .flush_entire_counter
                .fetch_add(1, Ordering::Relaxed);
        }

        // Send flush IPIs to the specified VPs.
        TdxTlbLockFlushAccess {
            vp_index: Some(self.vp.vp_index()),
            partition: self.vp.partition,
            shared: self.vp.shared,
        }
        .wake_processors_for_tlb_flush(vtl, (!flags.all_processors()).then_some(processor_set));

        // Mark that this VP needs to wait for all TLB locks to be released before returning.
        self.vp.set_wait_for_tlb_locks(vtl);

        Ok(())
    }
}

impl<T: CpuIo> UhHypercallHandler<'_, '_, T, TdxBacked> {
    fn add_ranges_to_tlb_flush_list(
        flush_state: &TdxPartitionFlushState,
        gva_ranges: &[HvGvaRange],
        use_extended_range_format: bool,
    ) -> Result<(), ()> {
        // If there are more gvas than the list size there's no point in filling the list.
        if gva_ranges.len() > FLUSH_GVA_LIST_SIZE {
            return Err(());
        }

        if use_extended_range_format
            && gva_ranges
                .iter()
                .any(|range| range.as_extended().large_page())
        {
            // TDX does not provide a way to flush large page ranges,
            // we have to promote this request to a flush entire.
            return Err(());
        }

        flush_state
            .gva_list
            .write()
            .extend(gva_ranges.iter().copied());

        Ok(())
    }
}

impl TdxTlbLockFlushAccess<'_> {
    fn wake_processors_for_tlb_flush(
        &mut self,
        target_vtl: GuestVtl,
        processor_set: Option<ProcessorSet<'_>>,
    ) {
        match processor_set {
            Some(processors) => {
                self.wake_processors_for_tlb_flush_inner(target_vtl, processors);
            }
            None => self.wake_processors_for_tlb_flush_inner(
                target_vtl,
                0..(self.partition.vps.len() as u32),
            ),
        }
    }

    fn wake_processors_for_tlb_flush_inner(
        &mut self,
        target_vtl: GuestVtl,
        processors: impl IntoIterator<Item = u32>,
    ) {
        // Use SeqCst ordering to ensure that we are observing the most
        // up-to-date value from other VPs. Otherwise we might not send a
        // wake to a VP in a lower VTL, which could cause TLB lock holders
        // to be stuck waiting until the target_vp happens to switch into
        // VTL 2.
        // We use a single fence to avoid having to take a SeqCst load
        // for each VP.
        std::sync::atomic::fence(Ordering::SeqCst);
        self.partition.hcl.kick_cpus(
            processors.into_iter().filter(|&vp| {
                self.shared.active_vtl[vp as usize].load(Ordering::Relaxed) == target_vtl as u8
            }),
            true,
            true,
        );
    }
}

struct TdxTlbLockFlushAccess<'a> {
    vp_index: Option<VpIndex>,
    partition: &'a UhPartitionInner,
    shared: &'a TdxBackedShared,
}

impl TlbFlushLockAccess for TdxTlbLockFlushAccess<'_> {
    fn flush(&mut self, vtl: GuestVtl) {
        self.shared.flush_state[vtl]
            .flush_entire_counter
            .fetch_add(1, Ordering::Relaxed);

        self.wake_processors_for_tlb_flush(vtl, None);
        self.set_wait_for_tlb_locks(vtl);
    }

    fn flush_entire(&mut self) {
        for vtl in [GuestVtl::Vtl0, GuestVtl::Vtl1] {
            self.shared.flush_state[vtl]
                .flush_entire_counter
                .fetch_add(1, Ordering::Relaxed);
        }
        for vtl in [GuestVtl::Vtl0, GuestVtl::Vtl1] {
            self.wake_processors_for_tlb_flush(vtl, None);
            self.set_wait_for_tlb_locks(vtl);
        }
    }

    fn set_wait_for_tlb_locks(&mut self, vtl: GuestVtl) {
        if let Some(vp_index) = self.vp_index {
            hardware_cvm::tlb_lock::TlbLockAccess {
                vp_index,
                cvm_partition: &self.shared.cvm,
            }
            .set_wait_for_tlb_locks(vtl);
        }
    }
}

mod save_restore {
    use super::TdxBacked;
    use super::UhProcessor;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;
    use vmcore::save_restore::SavedStateNotSupported;

    impl SaveRestore for UhProcessor<'_, TdxBacked> {
        type SavedState = SavedStateNotSupported;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Err(SaveError::NotSupported)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            match state {}
        }
    }
}
