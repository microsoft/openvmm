// Copyright (C) Microsoft Corporation. All rights reserved.

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
use crate::UhPartitionInner;
use crate::UhProcessor;
use crate::WakeReason;
use hcl::ioctl::tdx::Tdx;
use hcl::ioctl::ProcessorRunner;
use hcl::protocol::tdx_tdg_vp_enter_exit_info;
use hv1_hypercall::AsHandler;
use hv1_hypercall::HypercallIo;
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
use std::sync::Arc;
use thiserror::Error;
use tlb_flush::TdxFlushState;
use tlb_flush::TdxPartitionFlushState;
use tlb_flush::FLUSH_GVA_LIST_SIZE;
use virt::io::CpuIo;
use virt::state::StateElement;
use virt::vp;
use virt::vp::AccessVpState;
use virt::vp::Registers;
use virt::x86::MsrError;
use virt::x86::MsrErrorExt;
use virt::x86::SegmentRegister;
use virt::x86::TableRegister;
use virt::Processor;
use virt::VpHaltReason;
use virt::VpIndex;
use virt_support_apic::ApicClient;
use virt_support_apic::ApicWork;
use virt_support_apic::LocalApic;
use virt_support_apic::OffloadNotSupported;
use virt_support_x86emu::emulate::emulate_io;
use virt_support_x86emu::emulate::emulate_translate_gva;
use virt_support_x86emu::emulate::EmulatorSupport;
use virt_support_x86emu::emulate::TranslateGvaSupport;
use virt_support_x86emu::emulate::TranslateMode;
use virt_support_x86emu::translate::TranslationRegisters;
use vmcore::vmtime::VmTimeAccess;
use vtl_array::VtlArray;
use x86defs::apic::X2APIC_MSR_BASE;
use x86defs::cpuid::CpuidFunction;
use x86defs::tdx::TdCallResultCode;
use x86defs::tdx::TdVmCallR10Result;
use x86defs::tdx::TdxInstructionInfo;
use x86defs::tdx::TdxL2Ctls;
use x86defs::tdx::TdxL2EnterGuestState;
use x86defs::tdx::TdxVmFlags;
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
    fn new(reg: ShadowedRegister, initial_value: u64, allowed_bits: Option<u64>) -> Self {
        Self {
            register: reg,
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
        let old_physical_reg = runner.read_vmcs64(Vtl::Vtl0, self.register.physical_vmcs_field());

        tracing::trace!(old_physical_reg, "old_physical_reg");

        let guest_owned_mask = self.register.guest_owned_mask();
        if (old_physical_reg ^ value) & guest_owned_mask != 0 {
            let new_physical_reg =
                (old_physical_reg & !guest_owned_mask) | (value & guest_owned_mask);

            tracing::trace!(new_physical_reg, "new_physical_reg");

            runner.write_vmcs64(
                Vtl::Vtl0,
                self.register.physical_vmcs_field(),
                !0,
                new_physical_reg,
            );
        }

        self.shadow_value = value;
        runner.write_vmcs64(Vtl::Vtl0, self.register.shadow_vmcs_field(), !0, value);
        Ok(())
    }

    fn read(&self, runner: &ProcessorRunner<'_, Tdx>) -> u64 {
        let physical_reg = runner.read_vmcs64(Vtl::Vtl0, self.register.physical_vmcs_field());

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
    /// The EFER value for this VP.
    efer: u64,
    /// Virtual cr0.
    cr0: VirtualRegister,
    /// Virtual cr4.
    cr4: VirtualRegister,
    /// PFNs used for overlays.
    #[inspect(iter_by_index)]
    direct_overlays_pfns: [u64; UhDirectOverlay::Count as usize],
    #[inspect(skip)]
    #[allow(dead_code)] // Allocation handle for direct overlays held until drop
    direct_overlay_pfns_handle: shared_pool_alloc::SharedPoolHandle,

    lapic: LocalApic,
    #[inspect(with = "|x| inspect::iter_by_index(x).map_value(inspect::AsHex)")]
    eoi_exit_bitmap: [u64; 4],
    halted: bool,
    startup_suspend: bool,
    tpr_threshold: u8,
    nmi_pending: bool,
    #[inspect(skip)]
    processor_controls: ProcessorControls,
    #[inspect(skip)]
    secondary_processor_controls: SecondaryProcessorControls,
    #[inspect(skip)]
    interruption_information: InterruptionInformation,
    exception_error_code: u32,
    interruption_set: bool,

    /// TDX only TLB flush state.
    flush_state: VtlArray<TdxFlushState, 2>,
    /// A mapped page used for issuing INVGLA hypercalls.
    #[inspect(skip)]
    flush_page: shared_pool_alloc::SharedPoolHandle,

    enter_stats: EnterStats,
    exit_stats: ExitStats,

    shared: Arc<TdxBackedShared>,
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

impl HardwareIsolatedBacking for TdxBacked {}

/// Partition-wide shared data for TDX VPs.
#[derive(Inspect)]
pub struct TdxBackedShared {
    flush_state: VtlArray<RwLock<TdxPartitionFlushState>, 2>,
}

impl BackingPrivate for TdxBacked {
    type HclBacking = Tdx;
    type BackingShared = TdxBackedShared;

    fn new_shared_state(
        _params: BackingSharedParams<'_>,
    ) -> Result<Self::BackingShared, crate::Error> {
        Ok(TdxBackedShared {
            flush_state: VtlArray::from_fn(|_| RwLock::new(TdxPartitionFlushState::new())),
        })
    }

    fn new(params: super::private::BackingParams<'_, '_, Self>) -> Result<Self, crate::Error> {
        // TODO TDX: TDX shares the vp context page for xmm registers only. It
        // should probably move to its own page.
        //
        // FX regs and XMM registers are zero-initialized by the kernel. Set
        // them to the arch default.
        *params.runner.fx_state_mut() =
            vp::Xsave::at_reset(&params.partition.caps, params.vp_info).fxsave();

        let regs = Registers::at_reset(&params.partition.caps, params.vp_info);

        let TdxL2EnterGuestState {
            gps,
            rflags,
            rip,
            ssp: _,
            rvi: _,
            svi: _,
            reserved: _,
        } = params.runner.tdx_enter_guest_state_mut();

        *gps = [
            regs.rax, regs.rcx, regs.rdx, regs.rbx, regs.rsp, regs.rbp, regs.rsi, regs.rdi,
            regs.r8, regs.r9, regs.r10, regs.r11, regs.r12, regs.r13, regs.r14, regs.r15,
        ];
        *rflags = regs.rflags;
        *rip = regs.rip;

        // TODO TDX: ssp is for shadow stack

        // TODO TDX: direct overlay like snp?
        // TODO TDX: lapic / APIC setup?

        // TODO TDX: see ValInitializeVplc
        // TODO TDX: XCR_XFMEM setup?

        // Configure L2 controls to permit shared memory.
        let mut controls = TdxL2Ctls::new().with_enable_shared_ept(true);

        // If the synic is to be managed by the hypervisor, then enable TDVMCALLs.
        controls.set_enable_tdvmcall(params.partition.untrusted_synic.is_none());

        let hcl = &params.partition.hcl;

        params
            .runner
            .set_l2_ctls(Vtl::Vtl0, controls)
            .map_err(crate::Error::FailedToSetL2Ctls)?;

        // Set guest/host masks for CR0 and CR4. These enable shadowing these
        // registers since TDX requires certain bits to be set at all times.
        let initial_cr0 = params
            .runner
            .read_vmcs64(Vtl::Vtl0, VmcsField::VMX_VMCS_GUEST_CR0);
        assert_eq!(initial_cr0, X64_CR0_PE | X64_CR0_NE);

        // N.B. CR0.PE and CR0.PG are guest owned but still intercept when they
        // are changed for caching purposes and to ensure EFER is managed
        // properly due to the need to change execution state.
        params.runner.write_vmcs64(
            Vtl::Vtl0,
            VmcsField::VMX_VMCS_CR0_READ_SHADOW,
            !0,
            X64_CR0_PE | X64_CR0_NE,
        );
        params.runner.write_vmcs64(
            Vtl::Vtl0,
            VmcsField::VMX_VMCS_CR0_GUEST_HOST_MASK,
            !0,
            !ShadowedRegister::Cr0.guest_owned_mask() | X64_CR0_PE | X64_CR0_PG,
        );

        let initial_cr4 = params
            .runner
            .read_vmcs64(Vtl::Vtl0, VmcsField::VMX_VMCS_GUEST_CR4);
        assert_eq!(initial_cr4, X64_CR4_MCE | X64_CR4_VMXE);

        // Allowed cr4 bits depend on the values allowed by the SEAM.
        //
        // TODO TDX: Consider just using MSR kernel module instead of explicit
        // ioctl.
        let read_cr4 = hcl.read_vmx_cr4_fixed1();
        let allowed_cr4_bits = (ShadowedRegister::Cr4.guest_owned_mask() | X64_CR4_MCE) & read_cr4;

        params
            .runner
            .write_vmcs64(Vtl::Vtl0, VmcsField::VMX_VMCS_CR4_READ_SHADOW, !0, 0);
        params.runner.write_vmcs64(
            Vtl::Vtl0,
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
                    .write_msr_bitmap(Vtl::Vtl0, i as u32, !0, word);
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

        let mut lapic =
            params.partition.lapic.as_ref().unwrap()[Vtl::Vtl0].add_apic(params.vp_info);

        // Initialize APIC base to match the current VM state.
        let apic_base = vp::Apic::at_reset(&params.partition.caps, params.vp_info).apic_base;
        lapic.set_apic_base(apic_base).unwrap();

        // Cache the processor controls.
        let processor_controls = params
            .runner
            .read_vmcs32(Vtl::Vtl0, VmcsField::VMX_VMCS_PROCESSOR_CONTROLS)
            .into();

        // Cache the secondary processor controls.
        let secondary_processor_controls = params
            .runner
            .read_vmcs32(Vtl::Vtl0, VmcsField::VMX_VMCS_SECONDARY_PROCESSOR_CONTROLS)
            .into();

        // TODO: This needs to come from a private pool
        let flush_page = params
            .partition
            .shared_vis_pages_pool
            .as_ref()
            .expect("shared pool exists for cvm")
            .alloc(1.try_into().unwrap(), "tdx_tlb_flush".into())
            .expect("not out of memory");

        let crate::BackingShared::Tdx(shared) = params.backing_shared else {
            unreachable!()
        };

        Ok(Self {
            efer: regs.efer,
            cr0: VirtualRegister::new(ShadowedRegister::Cr0, regs.cr0, None),
            cr4: VirtualRegister::new(ShadowedRegister::Cr4, regs.cr4, Some(allowed_cr4_bits)),
            direct_overlays_pfns: overlays.try_into().unwrap(),
            direct_overlay_pfns_handle: pfns_handle,
            lapic,
            eoi_exit_bitmap: [0; 4],
            halted: false,
            nmi_pending: false,
            startup_suspend: !params.vp_info.base.is_bsp(),
            tpr_threshold: 0,
            processor_controls,
            secondary_processor_controls,
            interruption_information: Default::default(),
            interruption_set: false,
            exception_error_code: 0,
            flush_state: VtlArray::from_fn(|_| TdxFlushState::new()),
            flush_page,
            enter_stats: Default::default(),
            exit_stats: Default::default(),
            shared: shared.clone(),
        })
    }

    type StateAccess<'p, 'a> = UhVpStateAccess<'a, 'p, Self>
    where
        Self: 'a + 'p,
        'p: 'a;

    fn access_vp_state<'a, 'p>(
        this: &'a mut UhProcessor<'p, Self>,
        vtl: Vtl,
    ) -> Self::StateAccess<'p, 'a> {
        // TODO GUEST_VSM: VTL 1 access not supported yet.
        assert_eq!(vtl, Vtl::Vtl0);
        UhVpStateAccess::new(this, vtl)
    }

    fn init(this: &mut UhProcessor<'_, Self>) {
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

        let reg_count = if let Some(synic) = &mut this.untrusted_synic {
            synic.set_simp(
                &this.partition.gm[Vtl::Vtl0],
                reg(pfns[UhDirectOverlay::Sipp as usize]),
            );
            synic.set_siefp(
                &this.partition.gm[Vtl::Vtl0],
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
        this.set_apic_offload(true);
        this.backing.lapic.enable_offload();
    }

    async fn run_vp(
        this: &mut UhProcessor<'_, Self>,
        dev: &impl CpuIo,
        _stop: &mut virt::StopVp<'_>,
    ) -> Result<(), VpHaltReason<UhRunVpError>> {
        this.run_vp_tdx(dev).await
    }

    fn poll_apic(this: &mut UhProcessor<'_, Self>, scan_irr: bool) -> Result<bool, UhRunVpError> {
        if !this.try_poll_apic(scan_irr)? {
            tracing::info!("disabling APIC offload due to auto EOI");
            let page = zerocopy::transmute_mut!(this.runner.tdx_apic_page_mut());
            let (irr, isr) = pull_apic_offload(page);

            this.backing.lapic.disable_offload(&irr, &isr);
            this.set_apic_offload(false);
            this.try_poll_apic(false)?;
        }

        // Return ready even if halted. `run_vp` will wait in the kernel when
        // halted, which is necessary so that we are efficiently notified when
        // more interrupts arrive.
        Ok(true)
    }

    fn request_extint_readiness(_this: &mut UhProcessor<'_, Self>) {
        unreachable!("extint managed through software apic")
    }

    fn request_untrusted_sint_readiness(this: &mut UhProcessor<'_, Self>, sints: u16) {
        if let Some(synic) = &mut this.untrusted_synic {
            synic.request_sint_readiness(sints);
        } else {
            tracelimit::error_ratelimited!("untrusted synic is not configured");
        }
    }

    fn last_vtl(this: &UhProcessor<'_, Self>) -> Vtl {
        this.cvm_guest_vsm
            .as_ref()
            .map_or(Vtl::Vtl0, |gvsm_state| gvsm_state.current_vtl)
    }

    fn switch_vtl_state(_this: &mut UhProcessor<'_, Self>, _target_vtl: Vtl) {
        todo!()
    }
}

impl UhProcessor<'_, TdxBacked> {
    /// Returns `Ok(false)` if the APIC offload needs to be disabled and the
    /// poll retried.
    fn try_poll_apic(&mut self, scan_irr: bool) -> Result<bool, UhRunVpError> {
        // Check for interrupt requests from the host.
        let mut update_rvi = false;
        if let Some(irr) = self.runner.proxy_irr() {
            // TODO TDX: filter proxy IRRs.
            if self.backing.lapic.can_offload_irr() {
                // Put the proxied IRR directly on the APIC page to avoid going
                // through the local APIC.

                // OR in and update RVI.
                let page: &mut ApicPage = zerocopy::transmute_mut!(self.runner.tdx_apic_page_mut());
                for (page_irr, irr) in page.irr.iter_mut().zip(irr) {
                    page_irr.value |= irr;
                }
                update_rvi = true;
            } else {
                self.backing.lapic.request_fixed_interrupts(irr);
            }
        }

        let ApicWork {
            init,
            extint,
            sipi,
            nmi,
            interrupt,
        } = self.backing.lapic.scan(&mut self.vmtime, scan_irr);

        let mut new_processor_controls = self
            .backing
            .processor_controls
            .with_nmi_window_exiting(false)
            .with_interrupt_window_exiting(false);

        self.backing.nmi_pending |= nmi;
        if self.backing.nmi_pending {
            self.handle_nmi(&mut new_processor_controls);
        }

        if extint {
            tracelimit::warn_ratelimited!("extint not supported");
        }

        if init {
            self.handle_init()?;
        }

        if let Some(vector) = sipi {
            self.handle_sipi(vector);
        }

        let mut new_tpr_threshold = 0;
        if let Some(vector) = interrupt {
            self.handle_interrupt(vector, &mut new_processor_controls, &mut new_tpr_threshold);
        }

        if self.backing.tpr_threshold != new_tpr_threshold {
            tracing::trace!(new_tpr_threshold, "setting tpr threshold");
            self.runner.write_vmcs32(
                Vtl::Vtl0,
                VmcsField::VMX_VMCS_TPR_THRESHOLD,
                !0,
                new_tpr_threshold.into(),
            );
            self.backing.tpr_threshold = new_tpr_threshold;
        }

        if self.backing.processor_controls != new_processor_controls {
            tracing::debug!(?new_processor_controls, "requesting window change");
            self.runner.write_vmcs32(
                Vtl::Vtl0,
                VmcsField::VMX_VMCS_PROCESSOR_CONTROLS,
                !0,
                new_processor_controls.into(),
            );
            self.backing.processor_controls = new_processor_controls;
        }

        let r: Result<(), OffloadNotSupported> =
            self.backing.lapic.push_to_offload(|irr, isr, tmr| {
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
                self.runner.tdx_enter_guest_state_mut().svi = svi;
                update_rvi = true;

                // Ensure the EOI exit bitmap is up to date.
                let fields = [
                    VmcsField::VMX_VMCS_EOI_EXIT_0,
                    VmcsField::VMX_VMCS_EOI_EXIT_1,
                    VmcsField::VMX_VMCS_EOI_EXIT_2,
                    VmcsField::VMX_VMCS_EOI_EXIT_3,
                ];
                for ((&field, eoi_exit), tmr) in fields
                    .iter()
                    .zip(&mut self.backing.eoi_exit_bitmap)
                    .zip(tmr.chunks_exact(2))
                {
                    let tmr = tmr[0] as u64 | ((tmr[1] as u64) << 32);
                    if *eoi_exit != tmr {
                        self.runner.write_vmcs64(Vtl::Vtl0, field, !0, tmr);
                        *eoi_exit = tmr;
                    }
                }
            });

        if let Err(OffloadNotSupported) = r {
            //  APIC needs offloading to be disabled to support auto-EOI. The caller
            // will disable offload and try again.
            return Ok(false);
        }

        if update_rvi {
            let page: &mut ApicPage = zerocopy::transmute_mut!(self.runner.tdx_apic_page_mut());
            let rvi = top_vector(&page.irr);
            self.runner.tdx_enter_guest_state_mut().rvi = rvi;
        }

        // If there is a pending interrupt, clear the halted state.
        if self.backing.halted
            && self.backing.lapic.is_offloaded()
            && self.runner.tdx_enter_guest_state().rvi != 0
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
            self.backing.halted = false;
        }

        Ok(true)
    }

    fn access_apic_without_offload<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let offloaded = self.backing.lapic.is_offloaded();
        if offloaded {
            let (irr, isr) =
                pull_apic_offload(zerocopy::transmute_mut!(self.runner.tdx_apic_page_mut()));
            self.backing.lapic.disable_offload(&irr, &isr);
        }
        let r = f(self);
        if offloaded {
            self.backing.lapic.enable_offload();
        }
        r
    }

    fn set_apic_offload(&mut self, offload: bool) {
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
            self.runner.write_msr_bitmap(
                Vtl::Vtl0,
                offset + X2APIC_MSR_BASE / 64,
                !0,
                !offload_bitmap,
            );
        }

        // Update virtual-interrupt delivery.
        if self
            .backing
            .secondary_processor_controls
            .virtual_interrupt_delivery()
            != offload
        {
            self.backing
                .secondary_processor_controls
                .set_virtual_interrupt_delivery(offload);
            self.runner.write_vmcs32(
                Vtl::Vtl0,
                VmcsField::VMX_VMCS_SECONDARY_PROCESSOR_CONTROLS,
                !0,
                self.backing.secondary_processor_controls.into(),
            );
        }

        // Clear any pending external interrupt when enabling the APIC offload.
        if offload
            && self.backing.interruption_information.interruption_type() == INTERRUPT_TYPE_EXTERNAL
        {
            self.backing.interruption_information.set_valid(false);
        }
    }

    fn handle_interrupt(
        &mut self,
        vector: u8,
        processor_controls: &mut ProcessorControls,
        tpr_threshold: &mut u8,
    ) {
        // If there is a higher-priority pending event of some kind, then
        // just request an exit after it has resolved, after which we will
        // try again.
        if self.backing.interruption_information.valid()
            && self.backing.interruption_information.interruption_type() != INTERRUPT_TYPE_EXTERNAL
        {
            processor_controls.set_interrupt_window_exiting(true);
            return;
        }

        // Ensure the interrupt is not blocked by RFLAGS.IF or interrupt shadow.
        let interruptibility: Interruptibility = self
            .runner
            .read_vmcs32(Vtl::Vtl0, VmcsField::VMX_VMCS_GUEST_INTERRUPTIBILITY)
            .into();

        let rflags = RFlags::from(self.runner.tdx_enter_guest_state().rflags);
        if !rflags.interrupt_enable()
            || interruptibility.blocked_by_sti()
            || interruptibility.blocked_by_movss()
        {
            processor_controls.set_interrupt_window_exiting(true);
            return;
        }

        let priority = vector >> 4;
        let apic: &ApicPage = zerocopy::transmute_ref!(self.runner.tdx_apic_page());
        if (apic.tpr.value as u8 >> 4) >= priority {
            *tpr_threshold = priority;
            return;
        }

        self.backing.interruption_information = InterruptionInformation::new()
            .with_valid(true)
            .with_vector(vector)
            .with_interruption_type(INTERRUPT_TYPE_EXTERNAL);

        self.backing.halted = false;
    }

    fn handle_nmi(&mut self, processor_controls: &mut ProcessorControls) {
        // If there is a higher-priority pending event of some kind, then
        // just request an exit after it has resolved, after which we will
        // try again.
        if self.backing.interruption_information.valid()
            && self.backing.interruption_information.interruption_type() != INTERRUPT_TYPE_EXTERNAL
        {
            processor_controls.set_nmi_window_exiting(true);
            return;
        }

        let interruptibility: Interruptibility = self
            .runner
            .read_vmcs32(Vtl::Vtl0, VmcsField::VMX_VMCS_GUEST_INTERRUPTIBILITY)
            .into();

        if interruptibility.blocked_by_nmi()
            || interruptibility.blocked_by_sti()
            || interruptibility.blocked_by_movss()
        {
            processor_controls.set_nmi_window_exiting(true);
            return;
        }

        self.backing.interruption_information = InterruptionInformation::new()
            .with_valid(true)
            .with_vector(2)
            .with_interruption_type(INTERRUPT_TYPE_NMI);

        self.backing.halted = false;
    }

    fn handle_init(&mut self) -> Result<(), UhRunVpError> {
        let vp_info = self.inner.vp_info;
        {
            let mut access = self.access_state(Vtl::Vtl0);
            vp::x86_init(&mut access, &vp_info).map_err(UhRunVpError::State)?;
        }
        Ok(())
    }

    fn handle_sipi(&mut self, vector: u8) {
        if self.backing.startup_suspend {
            let address = (vector as u64) << 12;
            self.write_segment(
                Vtl::Vtl0,
                TdxSegmentReg::Cs,
                SegmentRegister {
                    base: address,
                    limit: 0xffff,
                    selector: (address >> 4) as u16,
                    attributes: 0x9b,
                },
            )
            .unwrap();
            self.runner.tdx_enter_guest_state_mut().rip = 0;
            self.backing.startup_suspend = false;
            self.backing.halted = false;
        }
    }

    async fn run_vp_tdx(&mut self, dev: &impl CpuIo) -> Result<(), VpHaltReason<UhRunVpError>> {
        if self.backing.interruption_information.valid() {
            tracing::debug!(
                vector = self.backing.interruption_information.vector(),
                vp_index = self.vp_index().index(),
                "injecting interrupt"
            );

            self.runner.write_vmcs32(
                Vtl::Vtl0,
                VmcsField::VMX_VMCS_ENTRY_INTERRUPT_INFO,
                !0,
                self.backing.interruption_information.into(),
            );
            if self.backing.interruption_information.deliver_error_code() {
                self.runner.write_vmcs32(
                    Vtl::Vtl0,
                    VmcsField::VMX_VMCS_ENTRY_EXCEPTION_ERROR_CODE,
                    !0,
                    self.backing.exception_error_code,
                );
            }
            self.backing.interruption_set = true;
        } else if self.backing.interruption_set {
            self.runner
                .write_vmcs32(Vtl::Vtl0, VmcsField::VMX_VMCS_ENTRY_INTERRUPT_INFO, !0, 0);
            self.backing.interruption_set = false;
        }

        // We're about to return to VTL 0, so do any pending flushes, unlock our
        // TLB locks, and wait for any others we're supposed to.
        self.do_tlb_flush(Vtl::Vtl0);
        self.unlock_tlb_lock(Vtl::Vtl2);
        let tlb_halt = self.should_halt_for_tlb_unlock(Vtl::Vtl0);

        self.runner
            .set_halted(self.backing.halted || self.backing.startup_suspend || tlb_halt);

        // TODO GUEST_VSM: Probably need to set this to 2 occasionally
        self.runner.tdx_vp_entry_flags_mut().set_vm_index(1);

        let has_intercept = self
            .runner
            .run()
            .map_err(|e| VpHaltReason::Hypervisor(UhRunVpError::Run(e)))?;

        *self.runner.tdx_vp_entry_flags_mut() = TdxVmFlags::new();

        if !has_intercept {
            return Ok(());
        }

        let exit_info = TdxExit(self.runner.tdx_vp_enter_exit_info());

        // Result codes above PENDING_INTERRUPT indicate the L2 was never entered.
        if exit_info.code().tdx_exit() >= TdCallResultCode::PENDING_INTERRUPT {
            self.backing.enter_stats.pending_intr.increment();
            return Ok(());
        }

        // The L2 was entered, so process the exit.
        let stat = match exit_info.code().tdx_exit() {
            TdCallResultCode::SUCCESS => &mut self.backing.enter_stats.success,
            TdCallResultCode::L2_EXIT_HOST_ROUTED_ASYNC => {
                &mut self.backing.enter_stats.host_routed_async
            }
            TdCallResultCode::L2_EXIT_PENDING_INTERRUPT => {
                &mut self.backing.enter_stats.l2_exit_pending_intr
            }
            TdCallResultCode::L2_EXIT_HOST_ROUTED_TDVMCALL => {
                // This is expected, and means that the hypervisor completed a
                // TD.VMCALL from the L2 and has requested to resume the L2 to
                // the L1.
                //
                // There is nothing to do here.
                assert_eq!(exit_info.code().vmx_exit(), VmxExit::TDCALL);
                &mut self.backing.enter_stats.host_routed_td_vmcall
            }
            _ => panic!("unexpected tdx exit code {:?}", exit_info.code()),
        };

        stat.increment();
        self.handle_vmx_exit(dev).await?;
        Ok(())
    }

    async fn handle_vmx_exit(
        &mut self,
        dev: &impl CpuIo,
    ) -> Result<(), VpHaltReason<UhRunVpError>> {
        let exit_info = TdxExit(self.runner.tdx_vp_enter_exit_info());
        let next_interruption = exit_info.idt_vectoring_info();

        // Acknowledge the APIC interrupt/NMI if it was delivered.
        if self.backing.interruption_information.valid()
            && (!next_interruption.valid()
                || self.backing.interruption_information.interruption_type()
                    != next_interruption.interruption_type())
        {
            match self.backing.interruption_information.interruption_type() {
                INTERRUPT_TYPE_EXTERNAL if !self.backing.lapic.is_offloaded() => {
                    // This must be a pending APIC interrupt. Acknowledge it.
                    tracing::debug!(
                        vector = self.backing.interruption_information.vector(),
                        "acknowledging interrupt"
                    );
                    self.backing
                        .lapic
                        .acknowledge_interrupt(self.backing.interruption_information.vector());
                }
                INTERRUPT_TYPE_NMI => {
                    // This must be a pending NMI.
                    tracing::debug!("acknowledging NMI");
                    self.backing.nmi_pending = false;
                }
                _ => {}
            }
        }

        if self.backing.lapic.is_offloaded() {
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
                self.backing.interruption_information = next_interruption;
                self.backing.exception_error_code = exit_info.idt_vectoring_error_code();
                self.backing.exit_stats.needs_interrupt_reinject.increment();
            } else {
                self.backing.interruption_information = Default::default();
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
                self.backing.interruption_information = next_interruption;
                self.backing.exception_error_code = exit_info.idt_vectoring_error_code();
            } else {
                self.backing.interruption_information = Default::default();
            }
        }

        let mut breakpoint_debug_exception = false;
        let stat = match exit_info.code().vmx_exit() {
            VmxExit::IO_INSTRUCTION => {
                let io_qual = ExitQualificationIo::from(exit_info.qualification() as u32);

                if io_qual.is_string() || io_qual.rep_prefix() {
                    self.emulate(dev, self.backing.interruption_information.valid())
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

                    let mut rax = self.runner.tdx_enter_guest_state().rax();
                    emulate_io(
                        self.inner.vp_info.base.vp_index,
                        !io_qual.is_in(),
                        io_qual.port(),
                        &mut rax,
                        len,
                        dev,
                    )
                    .await;
                    self.runner.tdx_enter_guest_state_mut().set_rax(rax);

                    self.advance_to_next_instruction();
                }
                &mut self.backing.exit_stats.io
            }
            VmxExit::MSR_READ => {
                let enter_state = self.runner.tdx_enter_guest_state();
                let msr = enter_state.rcx() as u32;

                let result = self
                    .backing
                    .lapic
                    .access(&mut TdxApicClient {
                        partition: self.partition,
                        vmtime: &self.vmtime,
                        apic_page: zerocopy::transmute_mut!(self.runner.tdx_apic_page_mut()),
                        dev,
                    })
                    .msr_read(msr)
                    .or_else_if_unknown(|| self.read_msr(msr))
                    .or_else_if_unknown(|| self.read_msr_cvm(msr));

                let value = match result {
                    Ok(v) => Some(v),
                    Err(MsrError::Unknown) => match msr {
                        X86X_MSR_EFER => Some(self.backing.efer),
                        _ => {
                            tracelimit::error_ratelimited!(msr, "unknown tdx cvm msr read");
                            Some(0)
                        }
                    },
                    Err(MsrError::InvalidAccess) => None,
                };

                let inject_gp = if let Some(value) = value {
                    let enter_state = self.runner.tdx_enter_guest_state_mut();
                    enter_state.set_rax((value as u32).into());
                    enter_state.set_rdx(((value >> 32) as u32).into());
                    false
                } else {
                    true
                };

                if inject_gp {
                    self.inject_gpf();
                } else {
                    self.advance_to_next_instruction();
                }
                &mut self.backing.exit_stats.msr_read
            }
            VmxExit::MSR_WRITE => {
                let enter_state = self.runner.tdx_enter_guest_state();
                let msr = enter_state.rcx() as u32;
                let value =
                    (enter_state.rax() as u32 as u64) | ((enter_state.rdx() as u32 as u64) << 32);

                let result = self
                    .backing
                    .lapic
                    .access(&mut TdxApicClient {
                        partition: self.partition,
                        vmtime: &self.vmtime,
                        apic_page: zerocopy::transmute_mut!(self.runner.tdx_apic_page_mut()),
                        dev,
                    })
                    .msr_write(msr, value)
                    .or_else_if_unknown(|| self.write_msr(msr, value))
                    .or_else_if_unknown(|| self.write_msr_cvm(msr, value));

                let inject_gp = match result {
                    Ok(()) => false,
                    Err(MsrError::Unknown) => {
                        tracelimit::error_ratelimited!(msr, value, "unknown tdx cvm msr write");
                        false
                    }
                    Err(MsrError::InvalidAccess) => true,
                };

                if inject_gp {
                    self.inject_gpf();
                } else {
                    self.advance_to_next_instruction();
                }
                &mut self.backing.exit_stats.msr_write
            }
            VmxExit::CPUID => {
                let xss = self.runner.tdx_vp_state().msr_xss;
                let enter_state = self.runner.tdx_enter_guest_state();
                let leaf = enter_state.rax() as u32;
                let subleaf = enter_state.rcx() as u32;
                let xfem = self
                    .runner
                    .get_vp_register(HvX64RegisterName::Xfem)
                    .map_err(|err| VpHaltReason::Hypervisor(UhRunVpError::EmulationState(err)))?
                    .as_u64();
                let guest_state = crate::hardware_cvm::cpuid::CpuidGuestState {
                    xfem,
                    xss,
                    cr4: self.backing.cr4.read(&self.runner),
                    apic_id: self.inner.vp_info.apic_id,
                };

                let result = self.partition.cvm.as_ref().unwrap().cpuid.guest_result(
                    CpuidFunction(leaf),
                    subleaf,
                    &guest_state,
                );

                tracing::trace!(leaf, subleaf, "cpuid");

                let [eax, ebx, ecx, edx] = self.partition.cpuid.lock().result(
                    leaf,
                    subleaf,
                    &[result.eax, result.ebx, result.ecx, result.edx],
                );

                tracing::trace!(eax, ebx, ecx, edx, "cpuid result");

                let enter_state = self.runner.tdx_enter_guest_state_mut();
                enter_state.set_rax(eax.into());
                enter_state.set_rbx(ebx.into());
                enter_state.set_rcx(ecx.into());
                enter_state.set_rdx(edx.into());

                self.advance_to_next_instruction();
                &mut self.backing.exit_stats.cpuid
            }
            VmxExit::VMCALL_INSTRUCTION => {
                if exit_info.cpl() != 0 {
                    self.inject_gpf();
                } else {
                    let is_64bit = self.backing.cr0.read(&self.runner) & X64_CR0_PE != 0
                        && self.backing.efer & X64_EFER_LMA != 0;

                    let guest_memory = self.last_vtl_gm();
                    let handler = UhHypercallHandler {
                        vp: &mut *self,
                        bus: dev,
                        trusted: true,
                    };

                    UhHypercallHandler::TDX_DISPATCHER.dispatch(
                        guest_memory,
                        hv1_hypercall::X64RegisterIo::new(handler, is_64bit),
                    );
                }
                &mut self.backing.exit_stats.vmcall
            }
            VmxExit::HLT_INSTRUCTION => {
                self.backing.halted = true;

                // TODO: see lots of these exits while waiting at frontpage.
                // Probably expected, given we will still get L1 timer
                // interrupts?

                // Clear interrupt shadow.
                let mask = Interruptibility::new().with_blocked_by_sti(true);
                let value = Interruptibility::new().with_blocked_by_sti(false);
                self.runner.write_vmcs32(
                    Vtl::Vtl0,
                    VmcsField::VMX_VMCS_GUEST_INTERRUPTIBILITY,
                    mask.into(),
                    value.into(),
                );
                self.advance_to_next_instruction();
                &mut self.backing.exit_stats.hlt
            }
            VmxExit::CR_ACCESS => {
                let qual = CrAccessQualification::from(exit_info.qualification());
                let cr;
                let value;
                match qual.access_type() {
                    CR_ACCESS_TYPE_MOV_TO_CR => {
                        cr = qual.cr();
                        value =
                            self.runner.tdx_enter_guest_state().gps[qual.gp_register() as usize];
                    }
                    CR_ACCESS_TYPE_LMSW => {
                        cr = 0;
                        let cr0 = self.backing.cr0.read(&self.runner);
                        // LMSW updates the low four bits only.
                        value = (qual.lmsw_source_data() as u64 & 0xf) | (cr0 & !0xf);
                    }
                    access_type => unreachable!("not registered for cr access type {access_type}"),
                }
                let r = match cr {
                    0 => self.backing.cr0.write(value, &mut self.runner),
                    4 => self.backing.cr4.write(value, &mut self.runner),
                    cr => unreachable!("not registered for cr{cr} accesses"),
                };
                if r.is_ok() {
                    self.update_execution_mode().expect("BUGBUG");
                    self.advance_to_next_instruction();
                } else {
                    tracelimit::warn_ratelimited!(cr, value, "failed to write cr");
                    self.inject_gpf();
                }
                &mut self.backing.exit_stats.cr_access
            }
            VmxExit::XSETBV => {
                let enter_state = self.runner.tdx_enter_guest_state();
                if let Some(value) =
                    hardware_cvm::validate_xsetbv_exit(hardware_cvm::XsetbvExitInput {
                        rax: enter_state.rax(),
                        rcx: enter_state.rcx(),
                        rdx: enter_state.rdx(),
                        cr4: self.backing.cr4.read(&self.runner),
                        cpl: exit_info.cpl(),
                    })
                {
                    self.runner
                        .set_vp_register(HvX64RegisterName::Xfem, value.into())
                        .map_err(|err| {
                            VpHaltReason::Hypervisor(UhRunVpError::EmulationState(err))
                        })?;
                    self.advance_to_next_instruction();
                } else {
                    self.inject_gpf();
                }
                &mut self.backing.exit_stats.xsetbv
            }
            VmxExit::WBINVD_INSTRUCTION => {
                // TODO TDX: forward the request to the host via TD.VMCALL
                // see HvlRequestCacheFlush
                self.advance_to_next_instruction();
                &mut self.backing.exit_stats.wbinvd
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
                let is_readable_ram = self.guest_memory().check_gpa_readable(exit_info.gpa());
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
                                    Vtl::Vtl0,
                                    VmcsField::VMX_VMCS_GUEST_INTERRUPTIBILITY,
                                    mask.into(),
                                    value.into(),
                                )
                                .into();
                            assert!(!old_interruptibility.blocked_by_nmi());
                        }
                    }

                    // Emulate the access.
                    self.emulate(dev, self.backing.interruption_information.valid())
                        .await?;
                }

                &mut self.backing.exit_stats.ept_violation
            }
            VmxExit::TPR_BELOW_THRESHOLD => {
                // Loop around to reevaluate the APIC.
                &mut self.backing.exit_stats.tpr_below_threshold
            }
            VmxExit::INTERRUPT_WINDOW => {
                // Loop around to reevaluate the APIC.
                &mut self.backing.exit_stats.interrupt_window
            }
            VmxExit::NMI_WINDOW => {
                // Loop around to reevaluate pending NMIs.
                &mut self.backing.exit_stats.nmi_window
            }
            VmxExit::HW_INTERRUPT => {
                // Check if the interrupt was triggered by a hardware breakpoint.
                let debug_regs = self
                    .access_state(Vtl::Vtl0)
                    .debug_regs()
                    .expect("register query should not fail");

                // The lowest four bits of DR6 indicate which of the
                // four breakpoints triggered.
                breakpoint_debug_exception = debug_regs.dr6.trailing_zeros() < 4;
                &mut self.backing.exit_stats.hw_interrupt
            }
            VmxExit::SMI_INTR => &mut self.backing.exit_stats.smi_intr,
            VmxExit::PAUSE_INSTRUCTION => &mut self.backing.exit_stats.pause,
            VmxExit::TDCALL => {
                // If the proxy synic is local, then the host did not get this
                // instruction, and we need to handle it.
                if self.untrusted_synic.is_some() {
                    self.handle_tdvmcall(dev);
                }
                &mut self.backing.exit_stats.tdcall
            }
            VmxExit::TRIPLE_FAULT => return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 }),
            _ => {
                return Err(VpHaltReason::Hypervisor(UhRunVpError::UnknownVmxExit(
                    exit_info.code().vmx_exit(),
                )))
            }
        };
        stat.increment();

        // Breakpoint exceptions may return a non-fatal error.
        // We dispatch here to correctly increment the counter.
        if breakpoint_debug_exception {
            #[cfg(all(feature = "gdb", guest_arch = "x86_64"))]
            self.handle_debug_exception()?;
        }

        Ok(())
    }

    fn advance_to_next_instruction(&mut self) {
        let instr_info = TdxExit(self.runner.tdx_vp_enter_exit_info()).instr_info();
        let rip = &mut self.runner.tdx_enter_guest_state_mut().rip;
        *rip = rip.wrapping_add(instr_info.length().into());
    }

    fn inject_gpf(&mut self) {
        self.backing.interruption_information = InterruptionInformation::new()
            .with_valid(true)
            .with_vector(x86defs::Exception::GENERAL_PROTECTION_FAULT.0)
            .with_interruption_type(INTERRUPT_TYPE_HARDWARE_EXCEPTION)
            .with_deliver_error_code(true);
        self.backing.exception_error_code = 0;
    }

    fn handle_tdvmcall(&mut self, dev: &impl CpuIo) {
        let regs = self.runner.tdx_enter_guest_state();
        if regs.r10() == 0 {
            // Architectural VMCALL.
            let result = match VmxExit(regs.r11() as u32) {
                VmxExit::MSR_WRITE => {
                    let msr = regs.r12() as u32;
                    let value = regs.r13();
                    match self.write_tdvmcall_msr(msr, value) {
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
                    let msr = regs.r12() as u32;
                    match self.read_tdvmcall_msr(msr) {
                        Ok(value) => {
                            tracing::debug!(msr, value, "tdvmcall msr read");
                            self.runner.tdx_enter_guest_state_mut().set_r11(value);
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
            let regs = self.runner.tdx_enter_guest_state_mut();
            regs.set_r10(result.0);
            regs.rip = regs.rip.wrapping_add(4);
        } else {
            // This hypercall is normally handled by the hypervisor, so the gpas
            // given by the guest should all be shared. The hypervisor allows
            // gpas to be set with or without the shared gpa boundary bit, which
            // untrusted_dma_memory correctly models. Note that some Linux
            // guests will issue hypercalls without the boundary bit set,
            // whereas UEFI will issue with the bit set.
            let guest_memory = &self.partition.untrusted_dma_memory;
            let handler = UhHypercallHandler {
                vp: &mut *self,
                bus: dev,
                trusted: false,
            };

            UhHypercallHandler::TDCALL_DISPATCHER.dispatch(guest_memory, TdHypercall(handler));
        }
    }

    fn read_tdvmcall_msr(&mut self, msr: u32) -> Result<u64, MsrError> {
        let last_vtl = self.last_vtl();
        match msr {
            msr @ (hvdef::HV_X64_MSR_GUEST_OS_ID | hvdef::HV_X64_MSR_VP_INDEX) => {
                self.hv(last_vtl).unwrap().msr_read(msr)
            }
            _ => self
                .untrusted_synic
                .as_mut()
                .unwrap()
                .read_nontimer_msr(msr),
        }
    }

    fn write_tdvmcall_msr(&mut self, msr: u32, value: u64) -> Result<(), MsrError> {
        let last_vtl = self.last_vtl();
        match msr {
            msr @ hvdef::HV_X64_MSR_GUEST_OS_ID => {
                self.hv_mut(last_vtl).unwrap().msr_write(msr, value)?
            }
            _ => {
                self.untrusted_synic.as_mut().unwrap().write_nontimer_msr(
                    &self.partition.gm[Vtl::Vtl0],
                    msr,
                    value,
                )?;
                // Propagate sint MSR writes to the hypervisor as well
                // so that the hypervisor can directly inject events.
                if matches!(msr, hvdef::HV_X64_MSR_SINT0..=hvdef::HV_X64_MSR_SINT15) {
                    if let Err(err) = self.runner.set_vp_register(
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

    fn read_msr_cvm(&self, msr: u32) -> Result<u64, MsrError> {
        // TODO TDX: port remaining tdx and common values from PvlIm.c
        // PvlImHandleMsrIntercept
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
            x86defs::X86X_MSR_XSS => Ok(self.runner.tdx_vp_state().msr_xss),
            x86defs::X86X_IA32_MSR_MISC_ENABLE => Ok(hv1_emulator::x86::MISC_ENABLE.into()),
            x86defs::X86X_IA32_MSR_FEATURE_CONTROL => Ok(VMX_FEATURE_CONTROL_LOCKED),
            x86defs::X86X_MSR_CR_PAT => {
                let pat = self
                    .runner
                    .read_vmcs64(Vtl::Vtl0, VmcsField::VMX_VMCS_GUEST_PAT);
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

    fn write_msr_cvm(&mut self, msr: u32, value: u64) -> Result<(), MsrError> {
        let state = self.runner.tdx_vp_state_mut();

        match msr {
            X86X_MSR_EFER => {
                self.write_efer(value)
                    .map_err(|_| MsrError::InvalidAccess)?;
                self.update_execution_mode().expect("BUGBUG");
            }
            x86defs::X86X_MSR_STAR => state.msr_star = value,
            x86defs::X86X_MSR_CSTAR => {
                // CSTAR writes are ignored.
            }
            x86defs::X86X_MSR_LSTAR => state.msr_lstar = value,
            x86defs::X86X_MSR_SFMASK => state.msr_sfmask = value,
            x86defs::X86X_MSR_SYSENTER_CS => {
                self.runner.write_vmcs32(
                    Vtl::Vtl0,
                    VmcsField::VMX_VMCS_GUEST_SYSENTER_CS_MSR,
                    !0,
                    value as u32,
                );
            }
            x86defs::X86X_MSR_SYSENTER_EIP => {
                self.runner.write_vmcs64(
                    Vtl::Vtl0,
                    VmcsField::VMX_VMCS_GUEST_SYSENTER_EIP_MSR,
                    !0,
                    value,
                );
            }
            x86defs::X86X_MSR_SYSENTER_ESP => {
                self.runner.write_vmcs64(
                    Vtl::Vtl0,
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
                    .write_vmcs64(Vtl::Vtl0, VmcsField::VMX_VMCS_GUEST_PAT, !0, value);
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
        vtl: Vtl,
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

    fn read_segment(&self, vtl: Vtl, seg: TdxSegmentReg) -> SegmentRegister {
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

impl<T: CpuIo> EmulatorSupport for UhEmulationState<'_, '_, T, TdxBacked> {
    type Error = UhRunVpError;

    fn vp_index(&self) -> VpIndex {
        self.vp.vp_index()
    }

    fn vendor(&self) -> x86defs::cpuid::Vendor {
        self.vp.partition.caps.vendor
    }

    fn state(&mut self) -> Result<x86emu::CpuState, Self::Error> {
        let cr0 = self.vp.backing.cr0.read(&self.vp.runner);
        let efer = self.vp.backing.efer;
        let cs = TdxExit(self.vp.runner.tdx_vp_enter_exit_info()).cs();
        let enter_state = self.vp.runner.tdx_enter_guest_state();

        // TODO TDX: Only supports VTL0
        Ok(x86emu::CpuState {
            gps: enter_state.gps,
            segs: [
                self.vp.read_segment(Vtl::Vtl0, TdxSegmentReg::Es).into(),
                cs.into(),
                self.vp.read_segment(Vtl::Vtl0, TdxSegmentReg::Ss).into(),
                self.vp.read_segment(Vtl::Vtl0, TdxSegmentReg::Ds).into(),
                self.vp.read_segment(Vtl::Vtl0, TdxSegmentReg::Fs).into(),
                self.vp.read_segment(Vtl::Vtl0, TdxSegmentReg::Gs).into(),
            ],
            rip: enter_state.rip,
            rflags: enter_state.rflags.into(),
            cr0,
            efer,
        })
    }

    fn set_state(&mut self, state: x86emu::CpuState) -> Result<(), Self::Error> {
        // TODO: immutable true? copied from snp
        let x86emu::CpuState {
            gps,
            segs: _, // immutable
            rip,
            rflags,
            cr0: _,  // immutable
            efer: _, // immutable
        } = state;
        let enter_state = self.vp.runner.tdx_enter_guest_state_mut();

        enter_state.gps = gps;
        enter_state.rip = rip;
        enter_state.rflags = rflags.into(); // TODO: rflags means interrupt state changed??
        Ok(())
    }

    fn get_xmm(&mut self, reg: usize) -> Result<u128, Self::Error> {
        Ok(u128::from_ne_bytes(self.vp.runner.fx_state().xmm[reg]))
    }

    fn set_xmm(&mut self, reg: usize, value: u128) -> Result<(), Self::Error> {
        self.vp.runner.fx_state_mut().xmm[reg] = value.to_ne_bytes();
        Ok(())
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
        // TODO TDX: VTL1 not supported
        // Lock Vtl TLB
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
        emulate_translate_gva(self.vp, gva, mode)
    }

    fn inject_pending_event(&mut self, event_info: hvdef::HvX64PendingEvent) {
        assert!(event_info.reg_0.event_pending());
        assert_eq!(
            event_info.reg_0.event_type(),
            hvdef::HV_X64_PENDING_EVENT_EXCEPTION
        );
        let exception = HvX64PendingExceptionEvent::from(u128::from(event_info.reg_0));

        self.vp.backing.interruption_information = InterruptionInformation::new()
            .with_deliver_error_code(exception.deliver_error_code())
            .with_interruption_type(INTERRUPT_TYPE_HARDWARE_EXCEPTION)
            .with_vector(exception.vector() as u8)
            .with_valid(true);

        self.vp.backing.exception_error_code = exception.error_code();
    }

    fn is_gpa_mapped(&self, gpa: u64, write: bool) -> bool {
        // Ignore the VTOM address bit when checking, since memory is mirrored
        // across the VTOM.
        let vtom = self.vp.partition.caps.vtom.unwrap();
        debug_assert!(vtom == 0 || vtom.is_power_of_two());
        self.vp.partition.is_gpa_mapped(gpa & !vtom, write)
    }
}

impl TranslateGvaSupport for UhProcessor<'_, TdxBacked> {
    type Error = UhRunVpError;

    fn guest_memory(&self) -> &guestmem::GuestMemory {
        self.last_vtl_gm()
    }

    fn acquire_tlb_lock(&mut self) {
        self.set_tlb_lock(Vtl::Vtl2, self.last_vtl())
    }

    fn registers(&mut self) -> Result<TranslationRegisters, Self::Error> {
        let cr0 = self.backing.cr0.read(&self.runner);
        let cr4 = self.backing.cr4.read(&self.runner);
        let efer = self.backing.efer;
        let cr3 = self
            .runner
            .read_vmcs64(Vtl::Vtl0, VmcsField::VMX_VMCS_GUEST_CR3);
        let ss = self.read_segment(Vtl::Vtl0, TdxSegmentReg::Ss).into();
        let rflags = self.runner.tdx_enter_guest_state().rflags;
        Ok(TranslationRegisters {
            cr0,
            cr4,
            efer,
            cr3,
            ss,
            rflags,
            encryption_mode: virt_support_x86emu::translate::EncryptionMode::Vtom(
                self.partition.caps.vtom.unwrap(),
            ),
        })
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
    fn write_efer(&mut self, efer: u64) -> Result<(), vp_state::Error> {
        if efer & (X64_EFER_SVME | X64_EFER_FFXSR) != 0 {
            return Err(vp_state::Error::SetEfer(efer, "SVME or FFXSR set"));
        }

        // EFER.NXE must be 1.
        if efer & X64_EFER_NXE == 0 {
            return Err(vp_state::Error::SetEfer(efer, "NXE not set"));
        }

        // Update the local value of EFER and the VMCS.
        self.backing.efer = efer;
        self.runner
            .write_vmcs64(Vtl::Vtl0, VmcsField::VMX_VMCS_GUEST_EFER, !0, efer);
        Ok(())
    }

    /// Read CR0 that includes guest shadowed bits. This is the value the guest
    /// sees.
    fn read_cr0(&self) -> u64 {
        self.backing.cr0.read(&self.runner)
    }

    /// Write to the guest CR0.
    fn write_cr0(&mut self, value: u64) -> Result<(), vp_state::Error> {
        self.backing
            .cr0
            .write(value | X64_CR0_ET, &mut self.runner)
            .expect("BUGBUG map error");

        Ok(())
    }

    fn read_cr4(&self) -> u64 {
        self.backing.cr4.read(&self.runner)
    }

    fn write_cr4(&mut self, value: u64) -> Result<(), vp_state::Error> {
        self.backing
            .cr4
            .write(value, &mut self.runner)
            .expect("BUGBUG map error");

        Ok(())
    }

    fn write_table_register(
        &mut self,
        vtl: Vtl,
        table: TdxTableReg,
        reg: TableRegister,
    ) -> Result<(), vp_state::Error> {
        self.runner
            .write_vmcs64(vtl, table.base_code(), !0, reg.base);
        self.runner
            .write_vmcs32(vtl, table.limit_code(), !0, reg.limit.into());
        Ok(())
    }

    fn read_table_register(&self, vtl: Vtl, table: TdxTableReg) -> TableRegister {
        let base = self.runner.read_vmcs64(vtl, table.base_code());
        let limit = self.runner.read_vmcs32(vtl, table.limit_code());

        TableRegister {
            base,
            limit: limit as u16,
        }
    }

    /// Update execution mode when CR0 or EFER is changed.
    fn update_execution_mode(&mut self) -> Result<(), vp_state::Error> {
        let lme = self.backing.efer & X64_EFER_LME == X64_EFER_LME;
        let pg = self.read_cr0() & X64_CR0_PG == X64_CR0_PG;
        let efer_lma = self.backing.efer & X64_EFER_LMA == X64_EFER_LMA;
        let lma = lme && pg;

        if lma != efer_lma {
            // Flip only the LMA bit.
            let new_efer = self.backing.efer ^ X64_EFER_LMA;
            self.write_efer(new_efer)?;
        }

        let mut entry_controls = self
            .runner
            .read_vmcs32(Vtl::Vtl0, VmcsField::VMX_VMCS_ENTRY_CONTROLS);
        if lma {
            entry_controls |= VMX_ENTRY_CONTROL_LONG_MODE_GUEST;
        } else {
            entry_controls &= !VMX_ENTRY_CONTROL_LONG_MODE_GUEST;
        }

        self.runner.write_vmcs32(
            Vtl::Vtl0,
            VmcsField::VMX_VMCS_ENTRY_CONTROLS,
            !0,
            entry_controls,
        );
        Ok(())
    }
}

struct TdxApicClient<'a, T> {
    partition: &'a UhPartitionInner,
    apic_page: &'a mut ApicPage,
    dev: &'a T,
    vmtime: &'a VmTimeAccess,
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
        self.partition.vps[vp_index.index() as usize].wake(Vtl::Vtl0, WakeReason::INTCON);
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
        self.vp.runner.tdx_enter_guest_state().rip
    }

    fn set_rip(&mut self, rip: u64) {
        self.vp.runner.tdx_enter_guest_state_mut().rip = rip;
    }

    fn gp(&mut self, n: hv1_hypercall::X64HypercallRegister) -> u64 {
        let enter_state = self.vp.runner.tdx_enter_guest_state();
        match n {
            hv1_hypercall::X64HypercallRegister::Rax => enter_state.rax(),
            hv1_hypercall::X64HypercallRegister::Rcx => enter_state.rcx(),
            hv1_hypercall::X64HypercallRegister::Rdx => enter_state.rdx(),
            hv1_hypercall::X64HypercallRegister::Rbx => enter_state.rbx(),
            hv1_hypercall::X64HypercallRegister::Rsi => enter_state.rsi(),
            hv1_hypercall::X64HypercallRegister::Rdi => enter_state.rdi(),
            hv1_hypercall::X64HypercallRegister::R8 => enter_state.r8(),
        }
    }

    fn set_gp(&mut self, n: hv1_hypercall::X64HypercallRegister, value: u64) {
        let enter_state = self.vp.runner.tdx_enter_guest_state_mut();
        match n {
            hv1_hypercall::X64HypercallRegister::Rax => enter_state.set_rax(value),
            hv1_hypercall::X64HypercallRegister::Rcx => enter_state.set_rcx(value),
            hv1_hypercall::X64HypercallRegister::Rdx => enter_state.set_rdx(value),
            hv1_hypercall::X64HypercallRegister::Rbx => enter_state.set_rbx(value),
            hv1_hypercall::X64HypercallRegister::Rsi => enter_state.set_rsi(value),
            hv1_hypercall::X64HypercallRegister::Rdi => enter_state.set_rdi(value),
            hv1_hypercall::X64HypercallRegister::R8 => enter_state.set_r8(value),
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
            // hv1_hypercall::HvEnablePartitionVtl,
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
        let enter_state = self.vp.runner.tdx_enter_guest_state();

        tracing::trace!("not getting cr8, must read from apic page or apic tpr");
        let cr8 = 0;

        let cs = self.vp.read_segment(Vtl::Vtl0, TdxSegmentReg::Cs);
        let ds = self.vp.read_segment(Vtl::Vtl0, TdxSegmentReg::Ds);
        let es = self.vp.read_segment(Vtl::Vtl0, TdxSegmentReg::Es);
        let fs = self.vp.read_segment(Vtl::Vtl0, TdxSegmentReg::Fs);
        let gs = self.vp.read_segment(Vtl::Vtl0, TdxSegmentReg::Gs);
        let ss = self.vp.read_segment(Vtl::Vtl0, TdxSegmentReg::Ss);
        let tr = self.vp.read_segment(Vtl::Vtl0, TdxSegmentReg::Tr);
        let ldtr = self.vp.read_segment(Vtl::Vtl0, TdxSegmentReg::Ldtr);

        let gdtr = self.vp.read_table_register(Vtl::Vtl0, TdxTableReg::Gdtr);
        let idtr = self.vp.read_table_register(Vtl::Vtl0, TdxTableReg::Idtr);

        let cr0 = self.vp.read_cr0();
        let cr2 = self.vp.runner.tdx_vp_state().cr2;
        let cr3 = self
            .vp
            .runner
            .read_vmcs64(Vtl::Vtl0, VmcsField::VMX_VMCS_GUEST_CR3);
        let cr4 = self.vp.read_cr4();

        let efer = self.vp.backing.efer;

        Ok(Registers {
            rax: enter_state.rax(),
            rcx: enter_state.rcx(),
            rdx: enter_state.rdx(),
            rbx: enter_state.rbx(),
            rsp: enter_state.rsp(),
            rbp: enter_state.rbp(),
            rsi: enter_state.rsi(),
            rdi: enter_state.rdi(),
            r8: enter_state.r8(),
            r9: enter_state.r9(),
            r10: enter_state.r10(),
            r11: enter_state.r11(),
            r12: enter_state.r12(),
            r13: enter_state.r13(),
            r14: enter_state.r14(),
            r15: enter_state.r15(),
            rip: enter_state.rip,
            rflags: enter_state.rflags,
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

        let enter_state = self.vp.runner.tdx_enter_guest_state_mut();
        enter_state.set_rax(*rax);
        enter_state.set_rcx(*rcx);
        enter_state.set_rdx(*rdx);
        enter_state.set_rbx(*rbx);
        enter_state.set_rsp(*rsp);
        enter_state.set_rbp(*rbp);
        enter_state.set_rsi(*rsi);
        enter_state.set_rdi(*rdi);
        enter_state.set_r8(*r8);
        enter_state.set_r9(*r9);
        enter_state.set_r10(*r10);
        enter_state.set_r11(*r11);
        enter_state.set_r12(*r12);
        enter_state.set_r13(*r13);
        enter_state.set_r14(*r14);
        enter_state.set_r15(*r15);
        enter_state.rip = *rip;
        // BUGBUG: rflags set also updates interrupts in hcl
        enter_state.rflags = *rflags;

        // Set segment registers
        self.vp.write_segment(Vtl::Vtl0, TdxSegmentReg::Cs, *cs)?;
        self.vp.write_segment(Vtl::Vtl0, TdxSegmentReg::Ds, *ds)?;
        self.vp.write_segment(Vtl::Vtl0, TdxSegmentReg::Es, *es)?;
        self.vp.write_segment(Vtl::Vtl0, TdxSegmentReg::Fs, *fs)?;
        self.vp.write_segment(Vtl::Vtl0, TdxSegmentReg::Gs, *gs)?;
        self.vp.write_segment(Vtl::Vtl0, TdxSegmentReg::Ss, *ss)?;
        self.vp.write_segment(Vtl::Vtl0, TdxSegmentReg::Tr, *tr)?;
        self.vp
            .write_segment(Vtl::Vtl0, TdxSegmentReg::Ldtr, *ldtr)?;

        // Set table registers
        self.vp
            .write_table_register(Vtl::Vtl0, TdxTableReg::Gdtr, *gdtr)?;
        self.vp
            .write_table_register(Vtl::Vtl0, TdxTableReg::Idtr, *idtr)?;

        self.vp.write_cr0(*cr0)?;

        // CR2 is shared with the kernel, so set it in the VP run page which
        // will be set before lower VTL entry.
        self.vp.runner.tdx_vp_state_mut().cr2 = *cr2;

        self.vp
            .runner
            .write_vmcs64(Vtl::Vtl0, VmcsField::VMX_VMCS_GUEST_CR3, !0, *cr3);

        self.vp.write_cr4(*cr4)?;

        // BUGBUG: cr8 affects interrupts but hcl asserts setting this to false.
        // ignore for now
        tracing::trace!(cr8, "IGNORING cr8 set_registers");

        self.vp.write_efer(*efer)?;

        // Execution mode must be updated after setting EFER and CR0.
        self.vp.update_execution_mode()?;

        Ok(())
    }

    fn activity(&mut self) -> Result<vp::Activity, Self::Error> {
        let mp_state = if self.vp.backing.startup_suspend {
            vp::MpState::WaitForSipi
        } else if self.vp.backing.halted {
            vp::MpState::Halted
        } else {
            vp::MpState::Running
        };
        let interruptibility: Interruptibility = self
            .vp
            .runner
            .read_vmcs32(Vtl::Vtl0, VmcsField::VMX_VMCS_GUEST_INTERRUPTIBILITY)
            .into();
        Ok(vp::Activity {
            mp_state,
            nmi_pending: self.vp.backing.nmi_pending,
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
        let (halted, startup_suspend) = match mp_state {
            vp::MpState::Running => (false, false),
            vp::MpState::WaitForSipi => (false, true),
            vp::MpState::Halted => (true, false),
            vp::MpState::Idle => (false, false), // TODO TDX: idle support
        };
        self.vp.backing.halted = halted;
        self.vp.backing.startup_suspend = startup_suspend;
        self.vp.backing.nmi_pending = nmi_pending;
        let interruptibility = Interruptibility::new()
            .with_blocked_by_movss(interrupt_shadow)
            .with_blocked_by_nmi(nmi_masked);
        self.vp.runner.write_vmcs32(
            Vtl::Vtl0,
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
        self.vp
            .access_apic_without_offload(|vp| Ok(vp.backing.lapic.save()))
    }

    fn set_apic(&mut self, value: &vp::Apic) -> Result<(), Self::Error> {
        self.vp.access_apic_without_offload(|vp| {
            vp.backing
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
                .get_vp_register(HvX64RegisterName::Xfem)
                .unwrap()
                .as_u64(),
        })
    }

    fn set_xcr(&mut self, _value: &vp::Xcr0) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("xcr"))
    }

    fn xss(&mut self) -> Result<vp::Xss, Self::Error> {
        Ok(vp::Xss {
            value: self.vp.runner.tdx_vp_state().msr_xss,
        })
    }

    fn set_xss(&mut self, _value: &vp::Xss) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("xss"))
    }

    fn cache_control(&mut self) -> Result<vp::CacheControl, Self::Error> {
        let msr_cr_pat = self
            .vp
            .runner
            .read_vmcs64(Vtl::Vtl0, VmcsField::VMX_VMCS_GUEST_PAT);
        Ok(vp::CacheControl {
            msr_cr_pat,
            msr_mtrr_def_type: 0, // TODO TDX: MTRRs
            fixed: [0; 11],       // TODO TDX: MTRRs
            variable: [0; 16],    // TODO TDX: MTRRs
        })
    }

    fn set_cache_control(&mut self, value: &vp::CacheControl) -> Result<(), Self::Error> {
        // TODO TDX: SNP only sets PAT, ignores MTRRs?
        let vp::CacheControl {
            msr_cr_pat,
            msr_mtrr_def_type: _,
            fixed: _,
            variable: _,
        } = *value;
        self.vp
            .runner
            .write_vmcs64(Vtl::Vtl0, VmcsField::VMX_VMCS_GUEST_PAT, !0, msr_cr_pat);
        Ok(())
    }

    fn virtual_msrs(&mut self) -> Result<vp::VirtualMsrs, Self::Error> {
        let state = self.vp.runner.tdx_vp_state();

        let sysenter_cs = self
            .vp
            .runner
            .read_vmcs32(Vtl::Vtl0, VmcsField::VMX_VMCS_GUEST_SYSENTER_CS_MSR)
            .into();
        let sysenter_eip = self
            .vp
            .runner
            .read_vmcs64(Vtl::Vtl0, VmcsField::VMX_VMCS_GUEST_SYSENTER_EIP_MSR);
        let sysenter_esp = self
            .vp
            .runner
            .read_vmcs64(Vtl::Vtl0, VmcsField::VMX_VMCS_GUEST_SYSENTER_ESP_MSR);

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

        let state = self.vp.runner.tdx_vp_state_mut();
        state.msr_kernel_gs_base = kernel_gs_base;
        state.msr_star = star;
        state.msr_lstar = lstar;
        state.msr_sfmask = sfmask;

        self.vp.runner.write_vmcs32(
            Vtl::Vtl0,
            VmcsField::VMX_VMCS_GUEST_SYSENTER_CS_MSR,
            !0,
            sysenter_cs as u32,
        );
        self.vp.runner.write_vmcs64(
            Vtl::Vtl0,
            VmcsField::VMX_VMCS_GUEST_SYSENTER_EIP_MSR,
            !0,
            sysenter_eip,
        );
        self.vp.runner.write_vmcs64(
            Vtl::Vtl0,
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
            .read_vmcs64(Vtl::Vtl0, VmcsField::VMX_VMCS_GUEST_DR7);

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
            .set_vp_registers([
                (HvX64RegisterName::Dr0, dr0),
                (HvX64RegisterName::Dr1, dr1),
                (HvX64RegisterName::Dr2, dr2),
                (HvX64RegisterName::Dr3, dr3),
                (HvX64RegisterName::Dr6, dr6),
            ])
            .map_err(vp_state::Error::SetRegisters)?;

        self.vp
            .runner
            .write_vmcs64(Vtl::Vtl0, VmcsField::VMX_VMCS_GUEST_DR7, !0, dr7);

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
        let regs = self.0.vp.runner.tdx_enter_guest_state_mut();
        regs.set_r10(0);
        regs.rip = regs.rip.wrapping_add(4);
    }

    fn retry(&mut self, control: u64) {
        self.0
            .vp
            .runner
            .tdx_enter_guest_state_mut()
            .set_r10(control);

        self.set_result(hvdef::hypercall::HypercallOutput::from(HvError::Timeout).into());
    }

    fn control(&mut self) -> u64 {
        self.0.vp.runner.tdx_enter_guest_state().r10()
    }

    fn input_gpa(&mut self) -> u64 {
        self.0.vp.runner.tdx_enter_guest_state().rdx()
    }

    fn output_gpa(&mut self) -> u64 {
        self.0.vp.runner.tdx_enter_guest_state().r8()
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
        self.0.vp.runner.tdx_enter_guest_state_mut().set_r11(n);
    }

    fn fast_regs(&mut self, starting_pair_index: usize, buf: &mut [[u64; 2]]) {
        let regs = self.0.vp.runner.tdx_enter_guest_state();
        let fx_state = self.0.vp.runner.fx_state();
        for (i, [low, high]) in buf.iter_mut().enumerate() {
            let index = i + starting_pair_index;
            if index == 0 {
                *low = regs.rdx();
                *high = regs.r8();
            } else {
                let value = u128::from_ne_bytes(fx_state.xmm[index - 1]);
                *low = value as u64;
                *high = (value >> 64) as u64;
            }
        }
    }
}

impl<T: CpuIo> hv1_hypercall::RetargetDeviceInterrupt for UhHypercallHandler<'_, '_, T, TdxBacked> {
    fn retarget_interrupt(
        &mut self,
        device_id: u64,
        address: u64,
        data: u32,
        params: &hv1_hypercall::HvInterruptParameters<'_>,
    ) -> hvdef::HvResult<()> {
        self.hcvm_retarget_interrupt(
            device_id,
            address,
            data,
            params.vector,
            params.multicast,
            params.target_processors,
        )
    }
}

impl<T: CpuIo> hv1_hypercall::FlushVirtualAddressList for UhHypercallHandler<'_, '_, T, TdxBacked> {
    fn flush_virtual_address_list(
        &mut self,
        processor_set: Vec<u32>,
        flags: HvFlushFlags,
        gva_ranges: &[HvGvaRange],
    ) -> hvdef::HvRepResult {
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
    ) -> hvdef::HvRepResult {
        self.hcvm_validate_flush_inputs(&processor_set, flags, true)
            .map_err(|e| (e, 0))?;

        let vtl = self.vp.last_vtl();
        {
            let mut flush_state = self.vp.backing.shared.flush_state[vtl].write();

            // If there are too many provided gvas then promote this request to a flush entire.
            // TODO do we need the extended check? I don't think so
            if gva_ranges.len() > FLUSH_GVA_LIST_SIZE {
                if flags.non_global_mappings_only() {
                    flush_state.s.flush_entire_non_global_counter += 1;
                } else {
                    flush_state.s.flush_entire_counter += 1;
                }
            } else {
                for range in gva_ranges {
                    if flush_state.gva_list.len() == FLUSH_GVA_LIST_SIZE {
                        flush_state.gva_list.pop_back();
                    }
                    flush_state.gva_list.push_front(*range);
                    flush_state.s.gva_list_count += 1;
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
        let vtl = self.vp.last_vtl();

        {
            let mut flush_state = self.vp.backing.shared.flush_state[vtl].write();

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
    pub fn wake_processors_for_tlb_flush(&mut self, vtl: Vtl, processor_set: Option<Vec<u32>>) {
        // TODO: Add additional checks? HCL checks that VP is active and in target VTL
        if let Some(processors) = processor_set {
            for vp in processors {
                if self.vp.vp_index().index() != vp {
                    self.vp.partition.vps[vp as usize].wake(vtl, WakeReason::TLB_FLUSH);
                }
            }
        } else {
            for vp in self.vp.partition.vps.iter() {
                if self.vp.vp_index().index() != vp.cpu_index {
                    vp.wake(vtl, WakeReason::TLB_FLUSH);
                }
            }
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
