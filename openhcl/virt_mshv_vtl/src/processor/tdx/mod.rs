// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Processor support for TDX partitions.

mod tlb_flush;

use super::hardware_cvm;
use super::private::BackingPrivate;
use super::vp_state;
use super::vp_state::UhVpStateAccess;
use super::BackingSharedParams;
use super::HardwareIsolatedBacking;
use super::UhEmulationState;
use super::UhHypercallHandler;
use super::UhRunVpError;
use crate::BackingShared;
use crate::GuestVtl;
use crate::UhCvmPartitionState;
use crate::UhCvmVpState;
use crate::UhPartitionInner;
use crate::UhProcessor;
use crate::WakeReason;
use hcl::ioctl::tdx::Tdx;
use hcl::ioctl::tdx::TdxPrivateRegs;
use hcl::ioctl::ProcessorRunner;
use hcl::protocol::hcl_intr_offload_flags;
use hcl::protocol::tdx_tdg_vp_enter_exit_info;
use hv1_emulator::hv::ProcessorVtlHv;
use hv1_emulator::synic::ProcessorSynic;
use hv1_hypercall::AsHandler;
use hv1_hypercall::HvRepResult;
use hv1_hypercall::HypercallIo;
use hv1_structs::VtlArray;
use hvdef::hypercall::HvFlushFlags;
use hvdef::hypercall::HvGvaRange;
use hvdef::HvError;
use hvdef::HvSynicSimpSiefp;
use hvdef::HvX64PendingExceptionEvent;
use hvdef::HvX64RegisterName;
use hvdef::Vtl;
use hvdef::HV_PAGE_SIZE;
use inspect::Inspect;
use inspect::InspectMut;
use inspect_counters::Counter;
use parking_lot::RwLock;
use std::num::NonZeroU64;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use thiserror::Error;
use tlb_flush::TdxFlushState;
use tlb_flush::TdxPartitionFlushState;
use tlb_flush::FLUSH_GVA_LIST_SIZE;
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
use virt::Processor;
use virt::VpHaltReason;
use virt::VpIndex;
use virt_support_apic::ApicClient;
use virt_support_apic::OffloadNotSupported;
use virt_support_x86emu::emulate::emulate_io;
use virt_support_x86emu::emulate::emulate_translate_gva;
use virt_support_x86emu::emulate::EmulatorSupport as X86EmulatorSupport;
use virt_support_x86emu::emulate::TranslateMode;
use virt_support_x86emu::translate::TranslationRegisters;
use vmcore::vmtime::VmTimeAccess;
use x86defs::apic::X2APIC_MSR_BASE;
use x86defs::cpuid::CpuidFunction;
use x86defs::tdx::TdCallResultCode;
use x86defs::tdx::TdVmCallR10Result;
use x86defs::tdx::TdxGp;
use x86defs::tdx::TdxInstructionInfo;
use x86defs::tdx::TdxL2Ctls;
use x86defs::tdx::TdxVpEnterRaxResult;
use x86defs::vmx::ApicPage;
use x86defs::vmx::ApicRegister;
use x86defs::vmx::CrAccessQualification;
use x86defs::vmx::ExitQualificationIo;
use x86defs::vmx::Interruptibility;
use x86defs::vmx::InterruptionInformation;
use x86defs::vmx::ProcessorControls;
use x86defs::vmx::SecondaryProcessorControls;
use x86defs::vmx::VmcsField;
use x86defs::vmx::VmxEptExitQualification;
use x86defs::vmx::VmxExit;
use x86defs::vmx::CR_ACCESS_TYPE_LMSW;
use x86defs::vmx::CR_ACCESS_TYPE_MOV_TO_CR;
use x86defs::vmx::INTERRUPT_TYPE_EXTERNAL;
use x86defs::vmx::INTERRUPT_TYPE_HARDWARE_EXCEPTION;
use x86defs::vmx::INTERRUPT_TYPE_NMI;
use x86defs::vmx::IO_SIZE_16_BIT;
use x86defs::vmx::IO_SIZE_32_BIT;
use x86defs::vmx::IO_SIZE_8_BIT;
use x86defs::vmx::VMX_ENTRY_CONTROL_LONG_MODE_GUEST;
use x86defs::vmx::VMX_FEATURE_CONTROL_LOCKED;
use x86defs::RFlags;
use x86defs::X64_CR0_ET;
use x86defs::X64_CR0_NE;
use x86defs::X64_CR0_PE;
use x86defs::X64_CR0_PG;
use x86defs::X64_CR4_MCE;
use x86defs::X64_CR4_VMXE;
use x86defs::X64_EFER_FFXSR;
use x86defs::X64_EFER_LMA;
use x86defs::X64_EFER_LME;
use x86defs::X64_EFER_NXE;
use x86defs::X64_EFER_SVME;
use x86defs::X86X_MSR_EFER;
use x86emu::Gp;
use x86emu::Segment;

#[derive(Debug)]
struct TdxExit<'a>(&'a tdx_tdg_vp_enter_exit_info);

impl TdxExit<'_> {
    fn code(&self) -> TdxVpEnterRaxResult {
        self.0.rax.into()
    }
    fn qualification(&self) -> u64 {
        self.0.rcx
    }
    fn gla(&self) -> u64 {
        self.0.rdx
    }
    fn gpa(&self) -> u64 {
        self.0.r8
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
                    | x86defs::X64_CR4_UMIP
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
    allowed_bits: Option<u64>,
}

#[derive(Debug, Error)]
enum VirtualRegisterError {
    #[error("invalid value {0} for register {1}")]
    InvalidValue(u64, &'static str),
}

impl VirtualRegister {
    fn new(
        reg: ShadowedRegister,
        vtl: GuestVtl,
        initial_value: u64,
        allowed_bits: Option<u64>,
    ) -> Self {
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
    fn write(
        &mut self,
        value: u64,
        runner: &mut ProcessorRunner<'_, Tdx>,
    ) -> Result<(), VirtualRegisterError> {
        tracing::trace!(?self.register, value, "write virtual register");

        if value & !self.allowed_bits.unwrap_or(u64::MAX) != 0 {
            return Err(VirtualRegisterError::InvalidValue(
                value,
                self.register.name(),
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

    fn read(&self, runner: &ProcessorRunner<'_, Tdx>) -> u64 {
        let physical_reg = runner.read_vmcs64(self.vtl, self.register.physical_vmcs_field());

        // Get the bits owned by the host from the shadow and the bits owned by the
        // guest from the physical value.
        let guest_owned_mask = self.register.guest_owned_mask();
        (self.shadow_value & !self.register.guest_owned_mask()) | (physical_reg & guest_owned_mask)
    }
}

const BITMAP_SIZE: usize = HV_PAGE_SIZE as usize / 8;
/// Bitmap used to control MSR intercepts.
struct MsrBitmap {
    bitmap: [u64; BITMAP_SIZE],
}

impl MsrBitmap {
    fn new() -> Self {
        // Initialize the bitmap with the default behavior of all 1s, which
        // means intercept.
        let mut bitmap = [u64::MAX; BITMAP_SIZE];

        let mut clear_msr_bit = |msr_index: u32, write: bool| {
            let mut word_index = ((msr_index & 0xFFFF) / 64) as usize;

            if msr_index & 0x80000000 == 0x80000000 {
                assert!((0xC0000000..=0xC0001FFF).contains(&msr_index));
                word_index += 0x80;
            } else {
                assert!(msr_index <= 0x00001FFF);
            }

            if write {
                word_index += 0x100;
            }

            // Clear the specified bit
            bitmap[word_index] &= !(1 << (msr_index as u64 & 0x3F));
        };

        const ALLOWED_READ: &[u32] = &[
            x86defs::X86X_MSR_TSC,
            X86X_MSR_EFER,
            x86defs::X86X_MSR_STAR,
            x86defs::X86X_MSR_LSTAR,
            x86defs::X86X_MSR_SFMASK,
            x86defs::X86X_MSR_SYSENTER_CS,
            x86defs::X86X_MSR_SYSENTER_ESP,
            x86defs::X86X_MSR_SYSENTER_EIP,
        ];

        const ALLOWED_READ_WRITE: &[u32] = &[
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
            x86defs::X86X_MSR_TSC_AUX,
            x86defs::X86X_MSR_INTERRUPT_SSP_TABLE_ADDR,
            x86defs::X86X_IA32_MSR_XFD,
            x86defs::X86X_IA32_MSR_XFD_ERR,
        ];

        for &msr in ALLOWED_READ {
            clear_msr_bit(msr, false);
        }

        for &msr in ALLOWED_READ_WRITE {
            clear_msr_bit(msr, false);
            clear_msr_bit(msr, true);
        }

        Self { bitmap }
    }
}

/// Backing for TDX partitions.
#[derive(InspectMut)]
pub struct TdxBacked {
    #[inspect(mut)]
    vtls: VtlArray<TdxVtl, 2>,

    /// PFNs used for overlays.
    #[inspect(iter_by_index)]
    direct_overlays_pfns: [u64; UhDirectOverlay::Count as usize],
    #[inspect(skip)]
    #[allow(dead_code)] // Allocation handle for direct overlays held until drop
    direct_overlay_pfns_handle: page_pool_alloc::PagePoolHandle,

    untrusted_synic: Option<ProcessorSynic>,
    #[inspect(with = "|x| inspect::iter_by_index(x).map_value(inspect::AsHex)")]
    eoi_exit_bitmap: [u64; 4],

    /// A mapped page used for issuing INVGLA hypercalls.
    #[inspect(skip)]
    flush_page: page_pool_alloc::PagePoolHandle,

    cvm: UhCvmVpState,
}

#[derive(InspectMut)]
struct TdxVtl {
    /// The EFER value for this VP.
    efer: u64,
    /// Virtual cr0.
    cr0: VirtualRegister,
    /// Virtual cr4.
    cr4: VirtualRegister,

    tpr_threshold: u8,
    #[inspect(skip)]
    processor_controls: ProcessorControls,
    #[inspect(skip)]
    secondary_processor_controls: SecondaryProcessorControls,
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
    pub segs: [Option<SegmentRegister>; 6],
    pub cr0: Option<u64>,
}

#[derive(Inspect, Default)]
pub struct EnterStats {
    pub success: Counter,
    pub host_routed_async: Counter,
    pub l2_exit_pending_intr: Counter,
    pub pending_intr: Counter,
    pub host_routed_td_vmcall: Counter,
}

#[derive(Inspect, Default)]
pub struct ExitStats {
    pub io: Counter,
    pub msr_read: Counter,
    pub msr_write: Counter,
    pub ept_violation: Counter,
    pub cpuid: Counter,
    pub cr_access: Counter,
    pub xsetbv: Counter,
    pub tpr_below_threshold: Counter,
    pub interrupt_window: Counter,
    pub nmi_window: Counter,
    pub vmcall: Counter,
    pub smi_intr: Counter,
    pub wbinvd: Counter,
    pub hw_interrupt: Counter,
    pub tdcall: Counter,
    pub hlt: Counter,
    pub pause: Counter,
    pub needs_interrupt_reinject: Counter,
    pub exception: Counter,
}

/// The number of shared pages required per cpu.
pub const fn shared_pages_required_per_cpu() -> u64 {
    UhDirectOverlay::Count as u64
}

enum UhDirectOverlay {
    Sipp,
    Sifp,
    Count,
}

impl HardwareIsolatedBacking for TdxBacked {
    fn cvm_state_mut(&mut self) -> &mut UhCvmVpState {
        &mut self.cvm
    }

    fn cvm_partition_state(shared: &Self::Shared) -> &UhCvmPartitionState {
        &shared.cvm
    }

    fn switch_vtl_state(
        _this: &mut UhProcessor<'_, Self>,
        _source_vtl: GuestVtl,
        _target_vtl: GuestVtl,
    ) {
        // TODO TDX GUEST VSM
        todo!()
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
}

/// Partition-wide shared data for TDX VPs.
#[derive(Inspect)]
pub struct TdxBackedShared {
    cvm: UhCvmPartitionState,
    flush_state: VtlArray<RwLock<TdxPartitionFlushState>, 2>,
    #[inspect(iter_by_index)]
    active_vtl: Vec<AtomicU8>,
}

impl TdxBackedShared {
    pub fn new(params: BackingSharedParams) -> Result<Self, crate::Error> {
        Ok(Self {
            flush_state: VtlArray::from_fn(|_| RwLock::new(TdxPartitionFlushState::new())),
            cvm: params.cvm_state.unwrap(),
            // VPs start in VTL 2.
            active_vtl: std::iter::repeat_n(2, params.vp_count as usize)
                .map(AtomicU8::new)
                .collect(),
        })
    }
}

impl BackingPrivate for TdxBacked {
    type HclBacking = Tdx;
    type Shared = TdxBackedShared;
    type EmulationCache = TdxEmulationCache;

    fn shared(shared: &BackingShared) -> &Self::Shared {
        let BackingShared::Tdx(shared) = shared else {
            unreachable!()
        };
        shared
    }

    fn new(
        params: super::private::BackingParams<'_, '_, Self>,
        _shared: &TdxBackedShared,
    ) -> Result<Self, crate::Error> {
        // TODO TDX: TDX shares the vp context page for xmm registers only. It
        // should probably move to its own page.
        //
        // FX regs and XMM registers are zero-initialized by the kernel. Set
        // them to the arch default.
        *params.runner.fx_state_mut() =
            vp::Xsave::at_reset(&params.partition.caps, params.vp_info).fxsave();

        let regs = Registers::at_reset(&params.partition.caps, params.vp_info);

        let gps = params.runner.tdx_enter_guest_gps_mut();
        *gps = [
            regs.rax, regs.rcx, regs.rdx, regs.rbx, regs.rsp, regs.rbp, regs.rsi, regs.rdi,
            regs.r8, regs.r9, regs.r10, regs.r11, regs.r12, regs.r13, regs.r14, regs.r15,
        ];

        // TODO TDX: ssp is for shadow stack

        // TODO TDX: direct overlay like snp?
        // TODO TDX: lapic / APIC setup?

        // TODO TDX: see ValInitializeVplc
        // TODO TDX: XCR_XFMEM setup?

        // TODO TDX GUEST VSM: Presumably we need to duplicate much of this work
        // when VTL 1 is enabled.

        // Configure L2 controls to permit shared memory.

        let mut controls =
            TdxL2Ctls::new().with_enable_shared_ept(!params.partition.hide_isolation);

        // If the synic is to be managed by the hypervisor, then enable TDVMCALLs.
        controls.set_enable_tdvmcall(
            params.partition.untrusted_synic.is_none() && !params.partition.hide_isolation,
        );

        let hcl = &params.partition.hcl;

        params
            .runner
            .set_l2_ctls(GuestVtl::Vtl0, controls)
            .map_err(crate::Error::FailedToSetL2Ctls)?;

        // Set guest/host masks for CR0 and CR4. These enable shadowing these
        // registers since TDX requires certain bits to be set at all times.
        let initial_cr0 = params
            .runner
            .read_vmcs64(GuestVtl::Vtl0, VmcsField::VMX_VMCS_GUEST_CR0);
        assert_eq!(initial_cr0, X64_CR0_PE | X64_CR0_NE);

        // N.B. CR0.PE and CR0.PG are guest owned but still intercept when they
        // are changed for caching purposes and to ensure EFER is managed
        // properly due to the need to change execution state.
        params.runner.write_vmcs64(
            GuestVtl::Vtl0,
            VmcsField::VMX_VMCS_CR0_READ_SHADOW,
            !0,
            X64_CR0_PE | X64_CR0_NE,
        );
        params.runner.write_vmcs64(
            GuestVtl::Vtl0,
            VmcsField::VMX_VMCS_CR0_GUEST_HOST_MASK,
            !0,
            !ShadowedRegister::Cr0.guest_owned_mask() | X64_CR0_PE | X64_CR0_PG,
        );

        let initial_cr4 = params
            .runner
            .read_vmcs64(GuestVtl::Vtl0, VmcsField::VMX_VMCS_GUEST_CR4);
        assert_eq!(initial_cr4, X64_CR4_MCE | X64_CR4_VMXE);

        // Allowed cr4 bits depend on the values allowed by the SEAM.
        //
        // TODO TDX: Consider just using MSR kernel module instead of explicit
        // ioctl.
        let read_cr4 = hcl.read_vmx_cr4_fixed1();
        let allowed_cr4_bits = (ShadowedRegister::Cr4.guest_owned_mask() | X64_CR4_MCE) & read_cr4;

        params
            .runner
            .write_vmcs64(GuestVtl::Vtl0, VmcsField::VMX_VMCS_CR4_READ_SHADOW, !0, 0);
        params.runner.write_vmcs64(
            GuestVtl::Vtl0,
            VmcsField::VMX_VMCS_CR4_GUEST_HOST_MASK,
            !0,
            !(ShadowedRegister::Cr4.guest_owned_mask() & allowed_cr4_bits),
        );

        // Configure the MSR bitmap for this VP.  Since the default MSR bitmap
        // is all ones, only those values that are not all ones need to be set in
        // the TDX module.
        let bitmap = MsrBitmap::new();
        for (i, &word) in bitmap.bitmap.iter().enumerate() {
            if word != u64::MAX {
                params
                    .runner
                    .write_msr_bitmap(GuestVtl::Vtl0, i as u32, !0, word);
            }
        }

        // Allocate PFNs for direct overlays
        let pfns_handle = params
            .partition
            .shared_vis_pages_pool
            .as_ref()
            .expect("must have shared vis pool when using SNP")
            .alloc(
                NonZeroU64::new(shared_pages_required_per_cpu()).expect("is nonzero"),
                format!("direct overlay vp {}", params.vp_info.base.vp_index.index()),
            )
            .map_err(super::Error::AllocateSharedVisOverlay)?;
        let pfns = pfns_handle.base_pfn()..pfns_handle.base_pfn() + pfns_handle.size_pages();
        let overlays: Vec<_> = pfns.collect();

        let flush_page = params
            .partition
            .private_vis_pages_pool
            .as_ref()
            .expect("private pool exists for cvm")
            .alloc_with_mapping(1.try_into().unwrap(), "tdx_tlb_flush".into())
            .expect("not out of memory");

        let untrusted_synic = params
            .partition
            .untrusted_synic
            .as_ref()
            .map(|synic| synic.add_vp(params.vp_info.base.vp_index));

        // Set the exception bitmap for VTL0.
        if params.partition.intercept_debug_exceptions {
            if cfg!(feature = "gdb") {
                let initial_exception_bitmap = params
                    .runner
                    .read_vmcs32(GuestVtl::Vtl0, VmcsField::VMX_VMCS_EXCEPTION_BITMAP);

                let exception_bitmap =
                    initial_exception_bitmap | (1 << x86defs::Exception::DEBUG.0);

                params.runner.write_vmcs32(
                    GuestVtl::Vtl0,
                    VmcsField::VMX_VMCS_EXCEPTION_BITMAP,
                    !0,
                    exception_bitmap,
                );
            } else {
                return Err(super::Error::InvalidDebugConfiguration);
            }
        }

        Ok(Self {
            vtls: VtlArray::from_fn(|vtl| {
                let vtl: GuestVtl = vtl.try_into().unwrap();
                TdxVtl {
                    efer: regs.efer,
                    cr0: VirtualRegister::new(ShadowedRegister::Cr0, vtl, regs.cr0, None),
                    cr4: VirtualRegister::new(
                        ShadowedRegister::Cr4,
                        vtl,
                        regs.cr4,
                        Some(allowed_cr4_bits),
                    ),
                    tpr_threshold: 0,
                    processor_controls: params
                        .runner
                        .read_vmcs32(vtl, VmcsField::VMX_VMCS_PROCESSOR_CONTROLS)
                        .into(),
                    secondary_processor_controls: params
                        .runner
                        .read_vmcs32(vtl, VmcsField::VMX_VMCS_SECONDARY_PROCESSOR_CONTROLS)
                        .into(),
                    interruption_information: Default::default(),
                    exception_error_code: 0,
                    interruption_set: false,
                    flush_state: TdxFlushState::new(),
                    private_regs: TdxPrivateRegs::new(regs.rflags, regs.rip, vtl),
                    enter_stats: Default::default(),
                    exit_stats: Default::default(),
                }
            }),
            direct_overlays_pfns: overlays.try_into().unwrap(),
            direct_overlay_pfns_handle: pfns_handle,
            untrusted_synic,
            eoi_exit_bitmap: [0; 4],
            flush_page,
            cvm: UhCvmVpState::new(params.hv.unwrap(), params.lapics.unwrap()),
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
        // TODO TDX GUEST VSM: Presumably we need to duplicate much of this work
        // when VTL 1 is enabled.

        // Configure the synic overlays.
        let pfns = &this.backing.direct_overlays_pfns;
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
            synic.set_simp(
                &this.partition.gm[GuestVtl::Vtl0],
                reg(pfns[UhDirectOverlay::Sipp as usize]),
            );
            synic.set_siefp(
                &this.partition.gm[GuestVtl::Vtl0],
                reg(pfns[UhDirectOverlay::Sifp as usize]),
            );
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

        // Enable APIC offload by default.
        this.set_apic_offload(GuestVtl::Vtl0, true);
        this.backing.cvm.lapics[GuestVtl::Vtl0]
            .lapic
            .enable_offload();
    }

    async fn run_vp(
        this: &mut UhProcessor<'_, Self>,
        dev: &impl CpuIo,
        _stop: &mut virt::StopVp<'_>,
    ) -> Result<(), VpHaltReason<UhRunVpError>> {
        this.run_vp_tdx(dev).await
    }

    fn poll_apic(
        this: &mut UhProcessor<'_, Self>,
        vtl: GuestVtl,
        scan_irr: bool,
    ) -> Result<(), UhRunVpError> {
        if !this.try_poll_apic(vtl, scan_irr)? {
            // We only offload VTL 0 today.
            assert_eq!(vtl, GuestVtl::Vtl0);
            tracing::info!("disabling APIC offload due to auto EOI");
            let page = zerocopy::transmute_mut!(this.runner.tdx_apic_page_mut());
            let (irr, isr) = pull_apic_offload(page);

            this.backing.cvm.lapics[vtl]
                .lapic
                .disable_offload(&irr, &isr);
            this.set_apic_offload(vtl, false);
            this.try_poll_apic(vtl, false)?;
        }

        Ok(())
    }

    fn request_extint_readiness(_this: &mut UhProcessor<'_, Self>) {
        unreachable!("extint managed through software apic")
    }

    fn request_untrusted_sint_readiness(this: &mut UhProcessor<'_, Self>, sints: u16) {
        if let Some(synic) = &mut this.backing.untrusted_synic {
            synic.request_sint_readiness(sints);
        } else {
            tracelimit::error_ratelimited!("untrusted synic is not configured");
        }
    }

    fn handle_cross_vtl_interrupts(
        this: &mut UhProcessor<'_, Self>,
        _dev: &impl CpuIo,
    ) -> Result<bool, UhRunVpError> {
        // TODO TDX GUEST VSM
        this.hcvm_handle_cross_vtl_interrupts(|_this, _vtl, _check_rflags| false)
    }

    fn hv(&self, vtl: GuestVtl) -> Option<&ProcessorVtlHv> {
        Some(&self.cvm.hv[vtl])
    }

    fn hv_mut(&mut self, vtl: GuestVtl) -> Option<&mut ProcessorVtlHv> {
        Some(&mut self.cvm.hv[vtl])
    }

    fn untrusted_synic(&self) -> Option<&ProcessorSynic> {
        self.untrusted_synic.as_ref()
    }

    fn untrusted_synic_mut(&mut self) -> Option<&mut ProcessorSynic> {
        self.untrusted_synic.as_mut()
    }

    fn handle_vp_start_enable_vtl_wake(
        this: &mut UhProcessor<'_, Self>,
        vtl: GuestVtl,
    ) -> Result<(), UhRunVpError> {
        this.hcvm_handle_vp_start_enable_vtl(vtl)
    }
}

impl UhProcessor<'_, TdxBacked> {
    /// Returns `Ok(false)` if the APIC offload needs to be disabled and the
    /// poll retried.
    fn try_poll_apic(&mut self, vtl: GuestVtl, scan_irr: bool) -> Result<bool, UhRunVpError> {
        let mut scan = TdxApicScanner {
            processor_controls: self.backing.vtls[vtl]
                .processor_controls
                .with_nmi_window_exiting(false)
                .with_interrupt_window_exiting(false),
            vp: self,
            tpr_threshold: 0,
        };

        // TODO TDX: filter proxy IRRs by setting the `proxy_irr_blocked` field of the run page
        hardware_cvm::apic::poll_apic_core(&mut scan, vtl, scan_irr)?;

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
            tracing::debug!(?new_processor_controls, "requesting window change");
            self.runner.write_vmcs32(
                vtl,
                VmcsField::VMX_VMCS_PROCESSOR_CONTROLS,
                !0,
                new_processor_controls.into(),
            );
            self.backing.vtls[vtl].processor_controls = new_processor_controls;
        }

        // Offloading is only done with VTL 0 today.
        if vtl == GuestVtl::Vtl0 {
            let mut update_rvi = false;
            let r: Result<(), OffloadNotSupported> = self.backing.cvm.lapics[vtl]
                .lapic
                .push_to_offload(|irr, isr, tmr| {
                    let apic_page: &mut ApicPage =
                        zerocopy::transmute_mut!(self.runner.tdx_apic_page_mut());

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
                            self.runner.proxy_irr_exit_mut()[i * 2] = tmr as u32;
                            self.runner.proxy_irr_exit_mut()[i * 2 + 1] = (tmr >> 32) as u32;
                        }
                    }
                });

            if let Err(OffloadNotSupported) = r {
                // APIC needs offloading to be disabled to support auto-EOI. The caller
                // will disable offload and try again.
                return Ok(false);
            }

            if update_rvi {
                let page: &mut ApicPage = zerocopy::transmute_mut!(self.runner.tdx_apic_page_mut());
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

        Ok(true)
    }

    fn access_apic_without_offload<R>(
        &mut self,
        vtl: GuestVtl,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let offloaded = self.backing.cvm.lapics[vtl].lapic.is_offloaded();
        if offloaded {
            let (irr, isr) =
                pull_apic_offload(zerocopy::transmute_mut!(self.runner.tdx_apic_page_mut()));
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
        if self.backing.vtls[vtl]
            .secondary_processor_controls
            .virtual_interrupt_delivery()
            != offload
        {
            self.backing.vtls[vtl]
                .secondary_processor_controls
                .set_virtual_interrupt_delivery(offload);
            self.runner.write_vmcs32(
                vtl,
                VmcsField::VMX_VMCS_SECONDARY_PROCESSOR_CONTROLS,
                !0,
                self.backing.vtls[vtl].secondary_processor_controls.into(),
            );
        }

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

    fn handle_interrupt(&mut self, vtl: GuestVtl, vector: u8) -> Result<(), UhRunVpError> {
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
            return Ok(());
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
            return Ok(());
        }

        let priority = vector >> 4;
        let apic: &ApicPage = zerocopy::transmute_ref!(self.vp.runner.tdx_apic_page());
        if (apic.tpr.value as u8 >> 4) >= priority {
            self.tpr_threshold = priority;
            return Ok(());
        }

        self.vp.backing.vtls[vtl].interruption_information = InterruptionInformation::new()
            .with_valid(true)
            .with_vector(vector)
            .with_interruption_type(INTERRUPT_TYPE_EXTERNAL);

        self.vp.backing.cvm.lapics[vtl].activity = MpState::Running;
        Ok(())
    }

    fn handle_nmi(&mut self, vtl: GuestVtl) -> Result<(), UhRunVpError> {
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
            return Ok(());
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
            return Ok(());
        }

        self.vp.backing.vtls[vtl].interruption_information = InterruptionInformation::new()
            .with_valid(true)
            .with_vector(2)
            .with_interruption_type(INTERRUPT_TYPE_NMI);

        self.vp.backing.cvm.lapics[vtl].activity = MpState::Running;
        Ok(())
    }

    fn handle_sipi(&mut self, vtl: GuestVtl, cs: SegmentRegister) -> Result<(), UhRunVpError> {
        self.vp.write_segment(vtl, TdxSegmentReg::Cs, cs).unwrap();
        self.vp.backing.vtls[vtl].private_regs.rip = 0;
        self.vp.backing.cvm.lapics[vtl].activity = MpState::Running;

        Ok(())
    }
}

impl UhProcessor<'_, TdxBacked> {
    async fn run_vp_tdx(&mut self, dev: &impl CpuIo) -> Result<(), VpHaltReason<UhRunVpError>> {
        let next_vtl = self.backing.cvm.exit_vtl;

        if self.backing.vtls[next_vtl].interruption_information.valid() {
            tracing::debug!(
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

        // Turn on kernel interrupt handling if possible
        let offload_enabled = next_vtl == GuestVtl::Vtl0
            && self.backing.vtls[next_vtl]
                .secondary_processor_controls
                .virtual_interrupt_delivery()
            && self.backing.cvm.lapics[next_vtl].lapic.can_offload_irr();
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
            .map_err(|e| VpHaltReason::Hypervisor(UhRunVpError::Run(e)))?;

        // TLB flushes can only target lower VTLs, so it is fine to use a relaxed
        // ordering here. The worst that can happen is some spurious wakes, due
        // to another VP observing that this VP is still in a lower VTL.
        self.shared.active_vtl[self.vp_index().index() as usize].store(2, Ordering::Relaxed);

        let entered_from_vtl = next_vtl;
        self.runner
            .read_private_regs(&mut self.backing.vtls[entered_from_vtl].private_regs);
        // TODO: Remove this line once the kernel does it for us
        self.backing.vtls[entered_from_vtl]
            .private_regs
            .vp_entry_flags
            .set_invd_translations(0);

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
                assert_eq!(exit_info.code().vmx_exit(), VmxExit::TDCALL);
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
    ) -> Result<(), VpHaltReason<UhRunVpError>> {
        let exit_info = TdxExit(self.runner.tdx_vp_enter_exit_info());
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
                    tracing::debug!(
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
        let stat = match exit_info.code().vmx_exit() {
            VmxExit::IO_INSTRUCTION => {
                let io_qual = ExitQualificationIo::from(exit_info.qualification() as u32);

                if io_qual.is_string() || io_qual.rep_prefix() {
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
                    let len = match io_qual.access_size() {
                        IO_SIZE_8_BIT => 1,
                        IO_SIZE_16_BIT => 2,
                        IO_SIZE_32_BIT => 4,
                        _ => panic!(
                            "tdx module returned invalid io instr size {}",
                            io_qual.access_size()
                        ),
                    };

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
                &mut self.backing.vtls[intercepted_vtl].exit_stats.io
            }
            VmxExit::MSR_READ => {
                let msr = self.runner.tdx_enter_guest_gps()[TdxGp::RCX] as u32;

                let result = self.backing.cvm.lapics[intercepted_vtl]
                    .lapic
                    .access(&mut TdxApicClient {
                        partition: self.partition,
                        vmtime: &self.vmtime,
                        apic_page: zerocopy::transmute_mut!(self.runner.tdx_apic_page_mut()),
                        dev,
                        vtl: intercepted_vtl,
                    })
                    .msr_read(msr)
                    .or_else_if_unknown(|| self.read_msr(msr, intercepted_vtl))
                    .or_else_if_unknown(|| self.read_msr_cvm(msr, intercepted_vtl))
                    .or_else_if_unknown(|| match msr {
                        hvdef::HV_X64_MSR_GUEST_IDLE => {
                            self.backing.cvm.lapics[intercepted_vtl].activity = MpState::Idle;
                            self.clear_interrupt_shadow(intercepted_vtl);
                            Ok(0)
                        }
                        X86X_MSR_EFER => Ok(self.backing.vtls[intercepted_vtl].efer),
                        _ => Err(MsrError::Unknown),
                    });

                let value = match result {
                    Ok(v) => Some(v),
                    Err(MsrError::Unknown) => {
                        tracelimit::error_ratelimited!(msr, "unknown tdx cvm msr read");
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
            VmxExit::MSR_WRITE => {
                let gps = self.runner.tdx_enter_guest_gps();
                let msr = gps[TdxGp::RCX] as u32;
                let value =
                    (gps[TdxGp::RAX] as u32 as u64) | ((gps[TdxGp::RDX] as u32 as u64) << 32);

                let result = self.backing.cvm.lapics[intercepted_vtl]
                    .lapic
                    .access(&mut TdxApicClient {
                        partition: self.partition,
                        vmtime: &self.vmtime,
                        apic_page: zerocopy::transmute_mut!(self.runner.tdx_apic_page_mut()),
                        dev,
                        vtl: intercepted_vtl,
                    })
                    .msr_write(msr, value)
                    .or_else_if_unknown(|| self.write_msr(msr, value, intercepted_vtl))
                    .or_else_if_unknown(|| self.write_msr_cvm(msr, value, intercepted_vtl));

                let inject_gp = match result {
                    Ok(()) => false,
                    Err(MsrError::Unknown) => {
                        tracelimit::error_ratelimited!(msr, value, "unknown tdx cvm msr write");
                        false
                    }
                    Err(MsrError::InvalidAccess) => true,
                };

                if inject_gp {
                    self.inject_gpf(intercepted_vtl);
                } else {
                    self.advance_to_next_instruction(intercepted_vtl);
                }
                &mut self.backing.vtls[intercepted_vtl].exit_stats.msr_write
            }
            VmxExit::CPUID => {
                let xss = self.backing.vtls[intercepted_vtl].private_regs.msr_xss;
                let gps = self.runner.tdx_enter_guest_gps();
                let leaf = gps[TdxGp::RAX] as u32;
                let subleaf = gps[TdxGp::RCX] as u32;
                let xfem = self
                    .runner
                    .get_vp_register(intercepted_vtl, HvX64RegisterName::Xfem)
                    .map_err(|err| VpHaltReason::Hypervisor(UhRunVpError::EmulationState(err)))?
                    .as_u64();
                let guest_state = crate::cvm_cpuid::CpuidGuestState {
                    xfem,
                    xss,
                    cr4: self.backing.vtls[intercepted_vtl].cr4.read(&self.runner),
                    apic_id: self.inner.vp_info.apic_id,
                };

                let result =
                    self.shared
                        .cvm
                        .cpuid
                        .guest_result(CpuidFunction(leaf), subleaf, &guest_state);

                let [eax, ebx, ecx, edx] = self.partition.cpuid.lock().result(
                    leaf,
                    subleaf,
                    &[result.eax, result.ebx, result.ecx, result.edx],
                );

                let gps = self.runner.tdx_enter_guest_gps_mut();
                gps[TdxGp::RAX] = eax.into();
                gps[TdxGp::RBX] = ebx.into();
                gps[TdxGp::RCX] = ecx.into();
                gps[TdxGp::RDX] = edx.into();

                self.advance_to_next_instruction(intercepted_vtl);
                &mut self.backing.vtls[intercepted_vtl].exit_stats.cpuid
            }
            VmxExit::VMCALL_INSTRUCTION => {
                if exit_info.cpl() != 0 {
                    self.inject_gpf(intercepted_vtl);
                } else {
                    let is_64bit =
                        self.backing.vtls[intercepted_vtl].cr0.read(&self.runner) & X64_CR0_PE != 0
                            && self.backing.vtls[intercepted_vtl].efer & X64_EFER_LMA != 0;

                    let guest_memory = &self.partition.gm[intercepted_vtl];
                    let handler = UhHypercallHandler {
                        trusted: !self.partition.hide_isolation,
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
            VmxExit::HLT_INSTRUCTION => {
                self.backing.cvm.lapics[intercepted_vtl].activity = MpState::Halted;

                // TODO TDX: see lots of these exits while waiting at frontpage.
                // Probably expected, given we will still get L1 timer
                // interrupts?
                self.clear_interrupt_shadow(intercepted_vtl);
                self.advance_to_next_instruction(intercepted_vtl);
                &mut self.backing.vtls[intercepted_vtl].exit_stats.hlt
            }
            VmxExit::CR_ACCESS => {
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
                let r = match cr {
                    0 => self.backing.vtls[intercepted_vtl]
                        .cr0
                        .write(value, &mut self.runner),
                    4 => self.backing.vtls[intercepted_vtl]
                        .cr4
                        .write(value, &mut self.runner),
                    cr => unreachable!("not registered for cr{cr} accesses"),
                };
                if r.is_ok() {
                    self.update_execution_mode(intercepted_vtl).expect("BUGBUG");
                    self.advance_to_next_instruction(intercepted_vtl);
                } else {
                    tracelimit::warn_ratelimited!(cr, value, "failed to write cr");
                    self.inject_gpf(intercepted_vtl);
                }
                &mut self.backing.vtls[intercepted_vtl].exit_stats.cr_access
            }
            VmxExit::XSETBV => {
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
                    self.runner
                        .set_vp_register(intercepted_vtl, HvX64RegisterName::Xfem, value.into())
                        .map_err(|err| {
                            VpHaltReason::Hypervisor(UhRunVpError::EmulationState(err))
                        })?;
                    self.advance_to_next_instruction(intercepted_vtl);
                } else {
                    self.inject_gpf(intercepted_vtl);
                }
                &mut self.backing.vtls[intercepted_vtl].exit_stats.xsetbv
            }
            VmxExit::WBINVD_INSTRUCTION => {
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
            VmxExit::EPT_VIOLATION => {
                // TODO TDX: If this is an access to a shared gpa, we need to
                // check the intercept page to see if this is a real exit or
                // spurious. This exit is only real if the hypervisor has
                // delivered an intercept message for this GPA.
                //
                // However, at this point the kernel has cleared that
                // information so some kind of redesign is required to figure
                // this out.
                //
                // For now, we instead treat EPTs on readable RAM as spurious
                // and log appropriately. This check is also not entirely
                // sufficient, as it may be a write access where the page is
                // protected, but we don't yet support MNF/guest VSM so this is
                // okay enough.
                let is_readable_ram =
                    self.partition.gm[intercepted_vtl].check_gpa_readable(exit_info.gpa());
                if is_readable_ram {
                    tracelimit::warn_ratelimited!(
                        gpa = exit_info.gpa(),
                        "possible spurious EPT violation, ignoring"
                    );
                } else {
                    // If this was an EPT violation while handling an iret, and
                    // that iret cleared the NMI blocking state, restore it.
                    if !next_interruption.valid() {
                        let ept_info = VmxEptExitQualification::from(exit_info.qualification());
                        if ept_info.nmi_unmasking_due_to_iret() {
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
                        }
                    }

                    // Emulate the access.
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

                &mut self.backing.vtls[intercepted_vtl].exit_stats.ept_violation
            }
            VmxExit::TPR_BELOW_THRESHOLD => {
                // Loop around to reevaluate the APIC.
                &mut self.backing.vtls[intercepted_vtl]
                    .exit_stats
                    .tpr_below_threshold
            }
            VmxExit::INTERRUPT_WINDOW => {
                // Loop around to reevaluate the APIC.
                &mut self.backing.vtls[intercepted_vtl]
                    .exit_stats
                    .interrupt_window
            }
            VmxExit::NMI_WINDOW => {
                // Loop around to reevaluate pending NMIs.
                &mut self.backing.vtls[intercepted_vtl].exit_stats.nmi_window
            }
            VmxExit::HW_INTERRUPT => {
                // Check if the interrupt was triggered by a hardware breakpoint.
                let debug_regs = self
                    .access_state(intercepted_vtl.into())
                    .debug_regs()
                    .expect("register query should not fail");

                // The lowest four bits of DR6 indicate which of the
                // four breakpoints triggered.
                breakpoint_debug_exception = debug_regs.dr6.trailing_zeros() < 4;
                &mut self.backing.vtls[intercepted_vtl].exit_stats.hw_interrupt
            }
            VmxExit::SMI_INTR => &mut self.backing.vtls[intercepted_vtl].exit_stats.smi_intr,
            VmxExit::PAUSE_INSTRUCTION => &mut self.backing.vtls[intercepted_vtl].exit_stats.pause,
            VmxExit::TDCALL => {
                // If the proxy synic is local, then the host did not get this
                // instruction, and we need to handle it.
                if self.backing.untrusted_synic.is_some() {
                    assert_eq!(intercepted_vtl, GuestVtl::Vtl0);
                    self.handle_tdvmcall(dev, intercepted_vtl);
                } else if self.partition.hide_isolation {
                    // TDCALL is not valid when hiding isolation. Inject a #UD.
                    self.backing.vtls[intercepted_vtl].interruption_information =
                        InterruptionInformation::new()
                            .with_valid(true)
                            .with_vector(x86defs::Exception::INVALID_OPCODE.0)
                            .with_interruption_type(INTERRUPT_TYPE_HARDWARE_EXCEPTION);
                }
                &mut self.backing.vtls[intercepted_vtl].exit_stats.tdcall
            }
            VmxExit::EXCEPTION => {
                tracing::trace!(
                    "Caught Exception: {:?}",
                    exit_info._exit_interruption_info()
                );
                breakpoint_debug_exception = true;
                &mut self.backing.vtls[intercepted_vtl].exit_stats.exception
            }
            VmxExit::TRIPLE_FAULT => {
                return Err(VpHaltReason::TripleFault {
                    vtl: intercepted_vtl.into(),
                })
            }
            _ => {
                return Err(VpHaltReason::Hypervisor(UhRunVpError::UnknownVmxExit(
                    exit_info.code().vmx_exit(),
                )))
            }
        };
        stat.increment();

        // Breakpoint exceptions may return a non-fatal error.
        // We dispatch here to correctly increment the counter.
        if cfg!(feature = "gdb") && breakpoint_debug_exception {
            self.handle_debug_exception(intercepted_vtl)?;
        }

        Ok(())
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
            let result = match VmxExit(regs[TdxGp::R11] as u32) {
                VmxExit::MSR_WRITE => {
                    let msr = regs[TdxGp::R12] as u32;
                    let value = regs[TdxGp::R13];
                    match self.write_tdvmcall_msr(msr, value, intercepted_vtl) {
                        Ok(()) => {
                            tracing::debug!(msr, value, "tdvmcall msr write");
                            TdVmCallR10Result::SUCCESS
                        }
                        Err(err) => {
                            tracelimit::warn_ratelimited!(
                                msr,
                                value,
                                ?err,
                                "failed tdvmcall msr write"
                            );
                            TdVmCallR10Result::OPERAND_INVALID
                        }
                    }
                }
                VmxExit::MSR_READ => {
                    let msr = regs[TdxGp::R12] as u32;
                    match self.read_tdvmcall_msr(msr, intercepted_vtl) {
                        Ok(value) => {
                            tracing::debug!(msr, value, "tdvmcall msr read");
                            self.runner.tdx_enter_guest_gps_mut()[TdxGp::R11] = value;
                            TdVmCallR10Result::SUCCESS
                        }
                        Err(err) => {
                            tracelimit::warn_ratelimited!(msr, ?err, "failed tdvmcall msr read");
                            TdVmCallR10Result::OPERAND_INVALID
                        }
                    }
                }
                subfunction => {
                    tracelimit::warn_ratelimited!(
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
            msr @ hvdef::HV_X64_MSR_GUEST_OS_ID => {
                self.backing.cvm.hv[intercepted_vtl].msr_write(msr, value)?
            }
            _ => {
                // If we get here we must have an untrusted synic, as otherwise
                // we wouldn't be handling the TDVMCALL that ends up here. Therefore
                // this is fine to unwrap.
                self.backing
                    .untrusted_synic
                    .as_mut()
                    .unwrap()
                    .write_nontimer_msr(&self.partition.gm[intercepted_vtl], msr, value)?;
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
                            error = &err as &dyn std::error::Error,
                            "failed to set sint register"
                        );
                    }
                }
            }
        }

        Ok(())
    }

    fn read_msr_cvm(&self, msr: u32, vtl: GuestVtl) -> Result<u64, MsrError> {
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
            x86defs::X86X_MSR_CSTAR => {
                // CSTAR doesn't exist on TDX, but Windows likes to read it.
                // Just return 0 to silence the error message.
                Ok(0)
            }
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

            _ => Err(MsrError::Unknown),
        }
    }

    fn write_msr_cvm(&mut self, msr: u32, value: u64, vtl: GuestVtl) -> Result<(), MsrError> {
        let state = &mut self.backing.vtls[vtl].private_regs;

        match msr {
            X86X_MSR_EFER => {
                self.write_efer(vtl, value)
                    .map_err(|_| MsrError::InvalidAccess)?;
                self.update_execution_mode(vtl).unwrap();
            }
            x86defs::X86X_MSR_STAR => state.msr_star = value,
            x86defs::X86X_MSR_CSTAR => {
                // CSTAR writes are ignored.
            }
            x86defs::X86X_MSR_LSTAR => state.msr_lstar = value,
            x86defs::X86X_MSR_SFMASK => state.msr_sfmask = value,
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
            x86defs::X86X_MSR_XSS => {
                state.msr_xss = value;
            }
            x86defs::X86X_MSR_MC_UPDATE_PATCH_LEVEL => {
                // Writing zero on intel platforms is allowed and ignored.
                if value != 0 {
                    return Err(MsrError::InvalidAccess);
                }
            }
            x86defs::X86X_IA32_MSR_MISC_ENABLE => return Ok(()),
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

            x86defs::X86X_MSR_MTRR_DEF_TYPE => {} // Ignore writes to this MSR

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

        // TODO TDX: cache CS into last exit because last exit contains CS optionally?

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
}

impl<T: CpuIo> X86EmulatorSupport for UhEmulationState<'_, '_, T, TdxBacked> {
    type Error = UhRunVpError;

    fn vp_index(&self) -> VpIndex {
        self.vp.vp_index()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        // no cached registers are modifiable by the emulator for TDX
        Ok(())
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

    fn set_xmm(&mut self, index: usize, v: u128) -> Result<(), Self::Error> {
        self.vp.runner.fx_state_mut().xmm[index] = v.to_ne_bytes();
        Ok(())
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
        Some(TdxExit(self.vp.runner.tdx_vp_enter_exit_info()).gpa())
    }

    fn initial_gva_translation(&self) -> Option<virt_support_x86emu::emulate::InitialTranslation> {
        let exit_info = TdxExit(self.vp.runner.tdx_vp_enter_exit_info());
        let ept_info = VmxEptExitQualification::from(exit_info.qualification());

        if ept_info.gva_valid() {
            Some(virt_support_x86emu::emulate::InitialTranslation {
                gva: exit_info.gla(),
                gpa: exit_info.gpa(),
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
    ) -> Result<(), virt_support_x86emu::emulate::EmuCheckVtlAccessError<Self::Error>> {
        // Lock Vtl TLB
        // TODO TDX GUEST VSM: VTL1 not yet supported
        Ok(())
    }

    fn translate_gva(
        &mut self,
        gva: u64,
        mode: TranslateMode,
    ) -> Result<
        Result<
            virt_support_x86emu::emulate::EmuTranslateResult,
            virt_support_x86emu::emulate::EmuTranslateError,
        >,
        Self::Error,
    > {
        emulate_translate_gva(self, gva, mode)
    }

    fn inject_pending_event(&mut self, event_info: hvdef::HvX64PendingEvent) {
        assert!(event_info.reg_0.event_pending());
        assert_eq!(
            event_info.reg_0.event_type(),
            hvdef::HV_X64_PENDING_EVENT_EXCEPTION
        );
        let exception = HvX64PendingExceptionEvent::from(u128::from(event_info.reg_0));

        self.vp.backing.vtls[self.vtl].interruption_information = InterruptionInformation::new()
            .with_deliver_error_code(exception.deliver_error_code())
            .with_interruption_type(INTERRUPT_TYPE_HARDWARE_EXCEPTION)
            .with_vector(exception.vector() as u8)
            .with_valid(true);

        self.vp.backing.vtls[self.vtl].exception_error_code = exception.error_code();
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
                apic_page: zerocopy::transmute_mut!(self.vp.runner.tdx_apic_page_mut()),
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
                apic_page: zerocopy::transmute_mut!(self.vp.runner.tdx_apic_page_mut()),
                vtl: self.vtl,
            })
            .mmio_write(address, data);
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
            return Err(vp_state::Error::SetEfer(efer, "SVME or FFXSR set"));
        }

        // EFER.NXE must be 1.
        if efer & X64_EFER_NXE == 0 {
            return Err(vp_state::Error::SetEfer(efer, "NXE not set"));
        }

        // Update the local value of EFER and the VMCS.
        self.backing.vtls[vtl].efer = efer;
        self.runner
            .write_vmcs64(vtl, VmcsField::VMX_VMCS_GUEST_EFER, !0, efer);
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
            .expect("BUGBUG map error");

        Ok(())
    }

    fn read_cr4(&self, vtl: GuestVtl) -> u64 {
        self.backing.vtls[vtl].cr4.read(&self.runner)
    }

    fn write_cr4(&mut self, vtl: GuestVtl, value: u64) -> Result<(), vp_state::Error> {
        self.backing.vtls[vtl]
            .cr4
            .write(value, &mut self.runner)
            .expect("BUGBUG map error");

        Ok(())
    }

    fn write_table_register(
        &mut self,
        vtl: GuestVtl,
        table: TdxTableReg,
        reg: TableRegister,
    ) -> Result<(), vp_state::Error> {
        self.runner
            .write_vmcs64(vtl, table.base_code(), !0, reg.base);
        self.runner
            .write_vmcs32(vtl, table.limit_code(), !0, reg.limit.into());
        Ok(())
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
    fn update_execution_mode(&mut self, vtl: GuestVtl) -> Result<(), vp_state::Error> {
        let lme = self.backing.vtls[vtl].efer & X64_EFER_LME == X64_EFER_LME;
        let pg = self.read_cr0(vtl) & X64_CR0_PG == X64_CR0_PG;
        let efer_lma = self.backing.vtls[vtl].efer & X64_EFER_LMA == X64_EFER_LMA;
        let lma = lme && pg;

        if lma != efer_lma {
            // Flip only the LMA bit.
            let new_efer = self.backing.vtls[vtl].efer ^ X64_EFER_LMA;
            self.write_efer(vtl, new_efer)?;
        }

        let mut entry_controls = self
            .runner
            .read_vmcs32(vtl, VmcsField::VMX_VMCS_ENTRY_CONTROLS);
        if lma {
            entry_controls |= VMX_ENTRY_CONTROL_LONG_MODE_GUEST;
        } else {
            entry_controls &= !VMX_ENTRY_CONTROL_LONG_MODE_GUEST;
        }

        self.runner
            .write_vmcs32(vtl, VmcsField::VMX_VMCS_ENTRY_CONTROLS, !0, entry_controls);
        Ok(())
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
            // TODO TDX: copied from SNP, enable individually as needed.
            // hv1_hypercall::HvGetVpRegisters,
            // hv1_hypercall::HvSetVpRegisters,
            // hv1_hypercall::HvEnablePartitionVtl,
            // hv1_hypercall::HvX64EnableVpVtl,
            // hv1_hypercall::HvVtlCall,
            // hv1_hypercall::HvVtlReturn,
            // hv1_hypercall::HvModifyVtlProtectionMask,
            // hv1_hypercall::HvX64TranslateVirtualAddress,
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

        tracing::trace!("not getting cr8, must read from apic page or apic tpr");
        let cr8 = 0;

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

        let efer = self.vp.backing.vtls[self.vtl].efer;

        Ok(Registers {
            rax: gps[TdxGp::RAX],
            rcx: gps[TdxGp::RCX],
            rdx: gps[TdxGp::RDX],
            rbx: gps[TdxGp::RBX],
            rsp: gps[TdxGp::RSP],
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
            cr8,
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
        gps[TdxGp::RSP] = *rsp;
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
            .write_table_register(self.vtl, TdxTableReg::Gdtr, *gdtr)?;
        self.vp
            .write_table_register(self.vtl, TdxTableReg::Idtr, *idtr)?;

        self.vp.write_cr0(self.vtl, *cr0)?;

        // CR2 is shared with the kernel, so set it in the VP run page which
        // will be set before lower VTL entry.
        self.vp.runner.set_cr2(*cr2);

        self.vp
            .runner
            .write_vmcs64(self.vtl, VmcsField::VMX_VMCS_GUEST_CR3, !0, *cr3);

        self.vp.write_cr4(self.vtl, *cr4)?;

        // BUGBUG: cr8 affects interrupts but hcl asserts setting this to false.
        // ignore for now
        tracing::trace!(cr8, "IGNORING cr8 set_registers");

        self.vp.write_efer(self.vtl, *efer)?;

        // Execution mode must be updated after setting EFER and CR0.
        self.vp.update_execution_mode(self.vtl)?;

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

    fn set_xss(&mut self, _value: &vp::Xss) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("xss"))
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
            cstar: 0, // CSTAR is ignored on intel platforms
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
            cstar: _,
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
        Err(vp_state::Error::Unimplemented("tsc_aux"))
    }

    fn set_tsc_aux(&mut self, _value: &vp::TscAux) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("tsc_aux"))
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

impl<T: CpuIo> hv1_hypercall::FlushVirtualAddressList for UhHypercallHandler<'_, '_, T, TdxBacked> {
    fn flush_virtual_address_list(
        &mut self,
        processor_set: Vec<u32>,
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
        processor_set: Vec<u32>,
        flags: HvFlushFlags,
        gva_ranges: &[HvGvaRange],
    ) -> HvRepResult {
        self.hcvm_validate_flush_inputs(&processor_set, flags, true)
            .map_err(|e| (e, 0))?;

        let vtl = self.intercepted_vtl;
        {
            let mut flush_state = self.vp.shared.flush_state[vtl].write();

            // If we fail to add ranges to the list for any reason then promote this request to a flush entire.
            if let Err(()) = Self::add_ranges_to_tlb_flush_list(
                &mut flush_state,
                gva_ranges,
                flags.use_extended_range_format(),
            ) {
                if flags.non_global_mappings_only() {
                    flush_state.s.flush_entire_non_global_counter += 1;
                } else {
                    flush_state.s.flush_entire_counter += 1;
                }
            }
        }

        // Send flush IPIs to the specified VPs.
        self.wake_processors_for_tlb_flush(vtl, (!flags.all_processors()).then_some(processor_set));

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
        processor_set: Vec<u32>,
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
        processor_set: Vec<u32>,
        flags: HvFlushFlags,
    ) -> hvdef::HvResult<()> {
        self.hcvm_validate_flush_inputs(&processor_set, flags, false)?;
        let vtl = self.intercepted_vtl;

        {
            let mut flush_state = self.vp.shared.flush_state[vtl].write();

            // Set flush entire.
            if flags.non_global_mappings_only() {
                flush_state.s.flush_entire_non_global_counter += 1;
            } else {
                flush_state.s.flush_entire_counter += 1;
            }
        }

        // Send flush IPIs to the specified VPs.
        self.wake_processors_for_tlb_flush(vtl, (!flags.all_processors()).then_some(processor_set));

        // Mark that this VP needs to wait for all TLB locks to be released before returning.
        self.vp.set_wait_for_tlb_locks(vtl);

        Ok(())
    }
}

impl<T: CpuIo> UhHypercallHandler<'_, '_, T, TdxBacked> {
    fn add_ranges_to_tlb_flush_list(
        flush_state: &mut TdxPartitionFlushState,
        gva_ranges: &[HvGvaRange],
        use_extended_range_format: bool,
    ) -> Result<(), ()> {
        // If there are more gvas than the list size there's no point in filling the list.
        if gva_ranges.len() > FLUSH_GVA_LIST_SIZE {
            return Err(());
        }

        for range in gva_ranges {
            if use_extended_range_format && range.as_extended().large_page() {
                // TDX does not provide a way to flush large page ranges,
                // we have to promote this request to a flush entire.
                return Err(());
            }
            if flush_state.gva_list.len() == FLUSH_GVA_LIST_SIZE {
                flush_state.gva_list.pop_front();
            }
            flush_state.gva_list.push_back(*range);
            flush_state.s.gva_list_count += 1;
        }

        Ok(())
    }

    fn wake_processors_for_tlb_flush(
        &mut self,
        target_vtl: GuestVtl,
        processor_set: Option<Vec<u32>>,
    ) {
        match processor_set {
            Some(processors) => {
                self.wake_processors_for_tlb_flush_inner(
                    target_vtl,
                    processors.into_iter().map(|x| x as usize),
                );
            }
            None => {
                self.wake_processors_for_tlb_flush_inner(target_vtl, 0..self.vp.partition.vps.len())
            }
        }
    }

    fn wake_processors_for_tlb_flush_inner(
        &mut self,
        target_vtl: GuestVtl,
        processors: impl Iterator<Item = usize>,
    ) {
        // Use SeqCst ordering to ensure that we are observing the most
        // up-to-date value from other VPs. Otherwise we might not send a
        // wake to a VP in a lower VTL, which could cause TLB lock holders
        // to be stuck waiting until the target_vp happens to switch into
        // VTL 2.
        // We use a single fence to avoid having to take a SeqCst load
        // for each VP.
        std::sync::atomic::fence(Ordering::SeqCst);
        for target_vp in processors {
            if self.vp.vp_index().index() as usize != target_vp
                && self.vp.shared.active_vtl[target_vp].load(Ordering::Relaxed) == target_vtl as u8
            {
                self.vp.partition.vps[target_vp].wake_vtl2();
            }
        }

        // TODO TDX GUEST VSM: We need to wait here until all woken VPs actually enter VTL 2.
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
