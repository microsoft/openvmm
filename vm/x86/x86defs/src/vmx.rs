// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Intel VMX specific definitions.

// TODO: move VMX defs somewhere?

use bitfield_struct::bitfield;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

open_enum! {
    /// VMX basic exit reason
    pub enum VmxExitBasic: u16 {
        EXCEPTION = 0x0,
        HW_INTERRUPT = 0x1,
        TRIPLE_FAULT = 0x2,
        SMI_INTR = 0x6,
        INTERRUPT_WINDOW = 0x7,
        NMI_WINDOW = 0x8,
        PAUSE_INSTRUCTION = 0x28,
        CPUID = 0xA,
        HLT_INSTRUCTION = 0xC,
        VMCALL_INSTRUCTION = 0x12,
        CR_ACCESS = 0x1C,
        IO_INSTRUCTION = 0x1E,
        MSR_READ = 0x1F,
        MSR_WRITE = 0x20,
        BAD_GUEST_STATE = 0x21,
        TPR_BELOW_THRESHOLD = 0x2B,
        GDTR_OR_IDTR = 0x2E,
        LDTR_OR_TR = 0x2F,
        EPT_VIOLATION = 0x30,
        TIMER_EXPIRED = 0x34,
        WBINVD_INSTRUCTION = 0x36,
        XSETBV = 0x37,
        TDCALL = 0x4D,
    }
}

impl VmxExitBasic {
    pub(crate) const fn from_bits(value: u64) -> Self {
        Self(value as u16)
    }

    pub(crate) const fn into_bits(self) -> u64 {
        self.0 as u64
    }
}

/// VMX exit reason
#[bitfield(u32)]
#[derive(PartialEq, Eq)]
pub struct VmxExit {
    #[bits(16)]
    pub basic_reason: VmxExitBasic,
    #[bits(10)]
    rsvd_0: u64,
    #[bits(1)]
    pub bus_lock_preempted: bool,
    #[bits(1)]
    pub enclave_interruption: bool,
    #[bits(1)]
    pub pending_mtf: bool,
    #[bits(2)]
    rsvd_1: u8,
    #[bits(1)]
    pub vm_enter_failed: bool,
}

pub const VMX_ENTRY_CONTROL_LONG_MODE_GUEST: u32 = 0x00000200;
pub const VMX_FEATURE_CONTROL_LOCKED: u64 = 0x0000000000000001;

#[repr(u32)]
#[derive(Debug, PartialEq, Eq)]
pub enum FieldWidth {
    Width16 = 0,
    Width32 = 2,
    Width64 = 1,
    WidthNatural = 3, // 32 on X86, 64 on X64.
}

impl FieldWidth {
    const fn from_bits(value: u32) -> Self {
        match value {
            0 => FieldWidth::Width16,
            2 => FieldWidth::Width32,
            1 => FieldWidth::Width64,
            3 => FieldWidth::WidthNatural,
            _ => panic!("Invalid field width"),
        }
    }

    const fn into_bits(self) -> u32 {
        self as u32
    }
}

#[bitfield(u32)]
pub struct VmcsField {
    #[bits(1)]
    pub access_high: u32,
    #[bits(9)]
    pub index: u32,
    #[bits(2)]
    pub typ: u32,
    #[bits(1)]
    pub reserved: u32,
    #[bits(2)]
    pub field_width: FieldWidth,
    #[bits(17)]
    pub reserved2: u32,
}

impl VmcsField {
    pub const VMX_VMCS_ENTRY_CONTROLS: Self = Self(0x00004012);

    pub const VMX_VMCS_GUEST_CR0: Self = Self(0x00006800);
    pub const VMX_VMCS_GUEST_CR3: Self = Self(0x00006802);
    pub const VMX_VMCS_GUEST_CR4: Self = Self(0x00006804);
    pub const VMX_VMCS_GUEST_DR7: Self = Self(0x0000681A);

    pub const VMX_VMCS_GUEST_ES_SELECTOR: Self = Self(0x00000800);
    pub const VMX_VMCS_GUEST_ES_BASE: Self = Self(0x00006806);
    pub const VMX_VMCS_GUEST_ES_LIMIT: Self = Self(0x00004800);
    pub const VMX_VMCS_GUEST_ES_AR: Self = Self(0x00004814);

    pub const VMX_VMCS_GUEST_CS_SELECTOR: Self = Self(0x00000802);
    pub const VMX_VMCS_GUEST_CS_BASE: Self = Self(0x00006808);
    pub const VMX_VMCS_GUEST_CS_LIMIT: Self = Self(0x00004802);
    pub const VMX_VMCS_GUEST_CS_AR: Self = Self(0x00004816);

    pub const VMX_VMCS_GUEST_SS_SELECTOR: Self = Self(0x00000804);
    pub const VMX_VMCS_GUEST_SS_BASE: Self = Self(0x0000680A);
    pub const VMX_VMCS_GUEST_SS_LIMIT: Self = Self(0x00004804);
    pub const VMX_VMCS_GUEST_SS_AR: Self = Self(0x00004818);

    pub const VMX_VMCS_GUEST_DS_SELECTOR: Self = Self(0x00000806);
    pub const VMX_VMCS_GUEST_DS_BASE: Self = Self(0x0000680C);
    pub const VMX_VMCS_GUEST_DS_LIMIT: Self = Self(0x00004806);
    pub const VMX_VMCS_GUEST_DS_AR: Self = Self(0x0000481A);

    pub const VMX_VMCS_GUEST_FS_SELECTOR: Self = Self(0x00000808);
    pub const VMX_VMCS_GUEST_FS_BASE: Self = Self(0x0000680E);
    pub const VMX_VMCS_GUEST_FS_LIMIT: Self = Self(0x00004808);
    pub const VMX_VMCS_GUEST_FS_AR: Self = Self(0x0000481C);

    pub const VMX_VMCS_GUEST_GS_SELECTOR: Self = Self(0x0000080A);
    pub const VMX_VMCS_GUEST_GS_BASE: Self = Self(0x00006810);
    pub const VMX_VMCS_GUEST_GS_LIMIT: Self = Self(0x0000480A);
    pub const VMX_VMCS_GUEST_GS_AR: Self = Self(0x0000481E);

    pub const VMX_VMCS_GUEST_LDTR_SELECTOR: Self = Self(0x0000080C);
    pub const VMX_VMCS_GUEST_LDTR_BASE: Self = Self(0x00006812);
    pub const VMX_VMCS_GUEST_LDTR_LIMIT: Self = Self(0x0000480C);
    pub const VMX_VMCS_GUEST_LDTR_AR: Self = Self(0x00004820);

    pub const VMX_VMCS_GUEST_TR_SELECTOR: Self = Self(0x0000080E);
    pub const VMX_VMCS_GUEST_TR_BASE: Self = Self(0x00006814);
    pub const VMX_VMCS_GUEST_TR_LIMIT: Self = Self(0x0000480E);
    pub const VMX_VMCS_GUEST_TR_AR: Self = Self(0x00004822);

    pub const VMX_VMCS_GUEST_GDTR_BASE: Self = Self(0x00006816);
    pub const VMX_VMCS_GUEST_GDTR_LIMIT: Self = Self(0x00004810);

    pub const VMX_VMCS_GUEST_IDTR_BASE: Self = Self(0x00006818);
    pub const VMX_VMCS_GUEST_IDTR_LIMIT: Self = Self(0x00004812);

    pub const VMX_VMCS_GUEST_PAT: Self = Self(0x00002804);
    pub const VMX_VMCS_GUEST_EFER: Self = Self(0x00002806);

    pub const VMX_VMCS_VIRTUAL_APIC_PAGE: Self = Self(0x00002012);

    pub const VMX_VMCS_EOI_EXIT_0: Self = Self(0x0000201C);
    pub const VMX_VMCS_EOI_EXIT_1: Self = Self(0x0000201E);
    pub const VMX_VMCS_EOI_EXIT_2: Self = Self(0x00002020);
    pub const VMX_VMCS_EOI_EXIT_3: Self = Self(0x00002022);

    pub const VMX_VMCS_PROCESSOR_CONTROLS: Self = Self(0x00004002);
    pub const VMX_VMCS_EXCEPTION_BITMAP: Self = Self(0x00004004);
    pub const VMX_VMCS_ENTRY_INTERRUPT_INFO: Self = Self(0x00004016);
    pub const VMX_VMCS_ENTRY_EXCEPTION_ERROR_CODE: Self = Self(0x00004018);
    pub const VMX_VMCS_ENTRY_INSTRUCTION_LENGTH: Self = Self(0x0000401A);
    pub const VMX_VMCS_TPR_THRESHOLD: Self = Self(0x0000401C);
    pub const VMX_VMCS_SECONDARY_PROCESSOR_CONTROLS: Self = Self(0x0000401E);

    pub const VMX_VMCS_CR0_GUEST_HOST_MASK: Self = Self(0x00006000);
    pub const VMX_VMCS_CR4_GUEST_HOST_MASK: Self = Self(0x00006002);
    pub const VMX_VMCS_CR0_READ_SHADOW: Self = Self(0x00006004);
    pub const VMX_VMCS_CR4_READ_SHADOW: Self = Self(0x00006006);

    pub const VMX_VMCS_GUEST_INTERRUPTIBILITY: Self = Self(0x00004824);

    pub const VMX_VMCS_GUEST_SYSENTER_CS_MSR: Self = Self(0x0000482A);
    pub const VMX_VMCS_GUEST_SYSENTER_ESP_MSR: Self = Self(0x00006824);
    pub const VMX_VMCS_GUEST_SYSENTER_EIP_MSR: Self = Self(0x00006826);
}

#[bitfield(u32)]
pub struct VmxSegmentAttributes {
    #[bits(4)]
    pub typ: u32,
    #[bits(1)]
    pub user: u32,
    #[bits(2)]
    pub dpl: u32,
    pub present: bool,
    #[bits(4)]
    pub reserved1: u32,
    pub available: bool,
    #[bits(1)]
    pub long_mode: u32,
    #[bits(1)]
    pub default_size: u32,
    #[bits(1)]
    pub granularity: u32,
    pub null: bool,
    #[bits(15)]
    pub reserved: u32,
}

pub const IO_SIZE_8_BIT: u8 = 0;
pub const IO_SIZE_16_BIT: u8 = 1;
pub const IO_SIZE_32_BIT: u8 = 3;

#[bitfield(u32)]
pub struct ExitQualificationIo {
    #[bits(2)]
    pub access_size: u8,
    #[bits(1)]
    pub reserved1: u8,
    pub is_in: bool,
    pub is_string: bool,
    pub rep_prefix: bool,
    pub immediate_operand: bool,
    #[bits(9)]
    pub reserved2: u32,
    pub port: u16,
}

#[bitfield(u64)]
pub struct VmxEptExitQualification {
    #[bits(3)]
    pub access_mask: u8,
    #[bits(4)]
    pub ept_access_mask: u8,
    pub gva_valid: bool,
    pub caused_by_gpa_access: bool,
    pub gva_user: bool,
    pub gva_read_write: bool,
    pub gva_no_execute: bool,
    pub nmi_unmasking_due_to_iret: bool,
    pub shadow_stack: bool,
    pub ept_supervisor_shadow_stack: bool,
    #[bits(49)]
    pub reserved: u64,
}

#[bitfield(u64)]
pub struct CrAccessQualification {
    #[bits(4)]
    pub cr: u8,
    #[bits(2)]
    pub access_type: u8,
    pub lmsw_is_memory: bool,
    #[bits(1)]
    _reserved1: u8,
    #[bits(4)]
    pub gp_register: u8,
    #[bits(4)]
    _reserved2: u8,
    pub lmsw_source_data: u16,
    _reserved3: u32,
}

pub const CR_ACCESS_TYPE_MOV_TO_CR: u8 = 0;
pub const CR_ACCESS_TYPE_MOV_FROM_CR: u8 = 1;
pub const CR_ACCESS_TYPE_CLTS: u8 = 2;
pub const CR_ACCESS_TYPE_LMSW: u8 = 3;

#[bitfield(u32)]
pub struct InterruptionInformation {
    #[bits(8)]
    pub vector: u8,
    #[bits(3)]
    pub interruption_type: u8,
    pub deliver_error_code: bool,
    #[bits(19)]
    pub reserved: u32,
    pub valid: bool,
}

#[bitfield(u32)]
pub struct GdtrOrIdtrInstructionInfo {
    #[bits(2)]
    pub scaling: u8,
    #[bits(5)]
    _reserved1: u8,
    #[bits(3)]
    pub address_size: u8,
    _reserved2: bool,
    pub operand_size: bool,
    #[bits(3)]
    _reserved3: u8,
    #[bits(3)]
    pub segment_register: u8,
    #[bits(4)]
    pub index_register: u8,
    pub index_register_invalid: bool,
    #[bits(4)]
    pub base_register: u8,
    pub base_register_invalid: bool,
    #[bits(2)]
    pub instruction: GdtrOrIdtrInstruction,
    #[bits(2)]
    _reserved4: u8,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum GdtrOrIdtrInstruction {
    Sgdt = 0,
    Sidt = 1,
    Lgdt = 2,
    Lidt = 3,
}

impl GdtrOrIdtrInstruction {
    const fn from_bits(value: u8) -> Self {
        match value {
            0 => GdtrOrIdtrInstruction::Sgdt,
            1 => GdtrOrIdtrInstruction::Sidt,
            2 => GdtrOrIdtrInstruction::Lgdt,
            3 => GdtrOrIdtrInstruction::Lidt,
            _ => unreachable!(),
        }
    }

    const fn into_bits(self) -> u8 {
        self as u8
    }

    pub const fn is_load(self) -> bool {
        match self {
            GdtrOrIdtrInstruction::Lgdt | GdtrOrIdtrInstruction::Lidt => true,
            GdtrOrIdtrInstruction::Sgdt | GdtrOrIdtrInstruction::Sidt => false,
        }
    }
}

#[bitfield(u32)]
pub struct LdtrOrTrInstructionInfo {
    #[bits(2)]
    pub scaling: u8,
    _reserved1: bool,
    #[bits(4)]
    pub register_1: u8,
    #[bits(3)]
    pub address_size: u8,
    /// 0 - Memory, 1 - Register
    pub memory_or_register: bool,
    #[bits(4)]
    _reserved2: u8,
    #[bits(3)]
    pub segment_register: u8,
    #[bits(4)]
    pub index_register: u8,
    pub index_register_invalid: bool,
    #[bits(4)]
    pub base_register: u8,
    pub base_register_invalid: bool,
    #[bits(2)]
    pub instruction: LdtrOrTrInstruction,
    #[bits(2)]
    _reserved4: u8,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum LdtrOrTrInstruction {
    Sldt = 0,
    Str = 1,
    Lldt = 2,
    Ltr = 3,
}

impl LdtrOrTrInstruction {
    const fn from_bits(value: u8) -> Self {
        match value {
            0 => LdtrOrTrInstruction::Sldt,
            1 => LdtrOrTrInstruction::Str,
            2 => LdtrOrTrInstruction::Lldt,
            3 => LdtrOrTrInstruction::Ltr,
            _ => unreachable!(),
        }
    }

    const fn into_bits(self) -> u8 {
        self as u8
    }

    pub const fn is_load(self) -> bool {
        match self {
            LdtrOrTrInstruction::Lldt | LdtrOrTrInstruction::Ltr => true,
            LdtrOrTrInstruction::Sldt | LdtrOrTrInstruction::Str => false,
        }
    }
}

pub const INTERRUPT_TYPE_EXTERNAL: u8 = 0;
pub const INTERRUPT_TYPE_NMI: u8 = 2;
pub const INTERRUPT_TYPE_HARDWARE_EXCEPTION: u8 = 3;
pub const INTERRUPT_TYPE_SOFTWARE_INTERRUPT: u8 = 4;
pub const INTERRUPT_TYPE_PRIVILEGED_SOFTWARE_INTERRUPT: u8 = 5;
pub const INTERRUPT_TYPE_SOFTWARE_EXCEPTION: u8 = 6;

#[bitfield(u32)]
pub struct Interruptibility {
    pub blocked_by_sti: bool,
    pub blocked_by_movss: bool,
    pub blocked_by_smi: bool,
    pub blocked_by_nmi: bool,
    #[bits(28)]
    _reserved: u32,
}

#[bitfield(u32)]
#[derive(PartialEq, Eq)]
pub struct ProcessorControls {
    #[bits(2)]
    _reserved: u32,
    pub interrupt_window_exiting: bool,
    pub use_tsc_offsetting: bool,
    #[bits(3)]
    _reserved2: u32,
    pub hlt_exiting: bool,
    _reserved3: bool,
    pub invlpg_exiting: bool,
    pub mwait_exiting: bool,
    pub rdpmc_exiting: bool,
    pub rdtsc_exiting: bool,
    #[bits(2)]
    _reserved4: u32,
    pub cr3_load_exiting: bool,
    pub cr3_store_exiting: bool,
    pub activate_tertiary_controls: bool,
    _reserved5: bool,
    pub cr8_load_exiting: bool,
    pub cr8_store_exiting: bool,
    pub use_tpr_shadow: bool,
    pub nmi_window_exiting: bool,
    pub mov_dr_exiting: bool,
    pub unconditional_io_exiting: bool,
    pub use_io_bitmaps: bool,
    _reserved6: bool,
    pub monitor_trap_flag: bool,
    pub use_msr_bitmaps: bool,
    pub monitor_exiting: bool,
    pub pause_exiting: bool,
    pub activate_secondary_controls: bool,
}

#[bitfield(u32)]
pub struct SecondaryProcessorControls {
    pub virtualize_apic_accesses: bool,
    pub enable_ept: bool,
    pub descriptor_table_exiting: bool,
    pub enable_rdtscp: bool,
    pub virtualize_x2apic_mode: bool,
    pub enable_vpid: bool,
    pub wbinvd_exiting: bool,
    pub unrestricted_guest: bool,
    pub apic_register_virtualization: bool,
    pub virtual_interrupt_delivery: bool,
    pub pause_loop_exiting: bool,
    pub rdrand_exiting: bool,
    pub enable_invpcid: bool,
    pub enable_vmfunc: bool,
    pub vmcs_shadowing: bool,
    pub enable_encls_exiting: bool,
    pub rdseed_exiting: bool,
    pub enable_pml: bool,
    pub ept_violation_ve: bool,
    pub conceal_vmx_from_pt: bool,
    pub enable_xsaves_xrstors: bool,
    pub pasid_translation: bool,
    pub mode_based_execute_control: bool,
    pub sub_page_write_permissions: bool,
    pub pt_uses_guest_physical_addresses: bool,
    pub use_tsc_scaling: bool,
    pub enable_user_wait_and_pause: bool,
    pub enable_pconfig: bool,
    pub enable_enclv_exiting: bool,
    _reserved: bool,
    pub vmm_bus_lock_detection: bool,
    pub instruction_timeout: bool,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ApicRegister {
    pub value: u32,
    _reserved: [u32; 3],
}

#[repr(C)]
#[derive(Debug, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ApicPage {
    pub reserved_0: [ApicRegister; 2],
    pub id: ApicRegister,
    pub version: ApicRegister,
    pub reserved_4: [ApicRegister; 4],
    pub tpr: ApicRegister,
    pub apr: ApicRegister,
    pub ppr: ApicRegister,
    pub eoi: ApicRegister,
    pub rrd: ApicRegister,
    pub ldr: ApicRegister,
    pub dfr: ApicRegister,
    pub svr: ApicRegister,
    pub isr: [ApicRegister; 8],
    pub tmr: [ApicRegister; 8],
    pub irr: [ApicRegister; 8],
    pub esr: ApicRegister,
    pub reserved_29: [ApicRegister; 6],
    pub lvt_cmci: ApicRegister,
    pub icr: [ApicRegister; 2],
    pub lvt_timer: ApicRegister,
    pub lvt_thermal: ApicRegister,
    pub lvt_pmc: ApicRegister,
    pub lvt_lint0: ApicRegister,
    pub lvt_lint1: ApicRegister,
    pub lvt_error: ApicRegister,
    pub timer_icr: ApicRegister,
    pub timer_ccr: ApicRegister,
    pub reserved_3a: [ApicRegister; 4],
    pub timer_dcr: ApicRegister,
    pub reserved_3f: ApicRegister,
    pub reserved_40: [ApicRegister; 0xc0],
}
