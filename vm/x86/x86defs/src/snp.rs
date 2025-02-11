// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! AMD SEV-SNP specific definitions.

use bitfield_struct::bitfield;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

// Interruption Information Field
pub const SEV_INTR_TYPE_EXT: u32 = 0;
pub const SEV_INTR_TYPE_NMI: u32 = 2;
pub const SEV_INTR_TYPE_EXCEPT: u32 = 3;
pub const SEV_INTR_TYPE_SW: u32 = 4;

// Secrets page layout.
pub const REG_TWEAK_BITMAP_OFFSET: usize = 0x100;
pub const REG_TWEAK_BITMAP_SIZE: usize = 0x40;

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct SevEventInjectInfo {
    pub vector: u8,
    #[bits(3)]
    pub interruption_type: u32,
    pub deliver_error_code: bool,
    #[bits(19)]
    _rsvd1: u64,
    pub valid: bool,
    pub error_code: u32,
}

#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Vmpl {
    Vmpl0 = 0,
    Vmpl1 = 1,
    Vmpl2 = 2,
    Vmpl3 = 3,
}

impl From<Vmpl> for u8 {
    fn from(value: Vmpl) -> Self {
        value as _
    }
}

/// A X64 selector register.
#[repr(C)]
#[derive(Debug, Clone, Copy, IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct SevSelector {
    pub selector: u16,
    pub attrib: u16,
    pub limit: u32,
    pub base: u64,
}

impl SevSelector {
    pub fn as_u128(&self) -> u128 {
        ((self.base as u128) << 64)
            | ((self.limit as u128) << 32)
            | ((self.attrib as u128) << 16)
            | self.selector as u128
    }
}

impl From<u128> for SevSelector {
    fn from(val: u128) -> Self {
        SevSelector {
            selector: val as u16,
            attrib: (val >> 16) as u16,
            limit: (val >> 32) as u32,
            base: (val >> 64) as u64,
        }
    }
}

/// An X64 XMM register.
#[repr(C)]
#[derive(Debug, Clone, Copy, IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct SevXmmRegister {
    low: u64,
    high: u64,
}

impl SevXmmRegister {
    pub fn as_u128(&self) -> u128 {
        ((self.high as u128) << 64) | self.low as u128
    }
}

impl From<u128> for SevXmmRegister {
    fn from(val: u128) -> Self {
        SevXmmRegister {
            low: val as u64,
            high: (val >> 64) as u64,
        }
    }
}

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct SevFeatures {
    pub snp: bool,
    pub vtom: bool,
    pub reflect_vc: bool,
    pub restrict_injection: bool,
    pub alternate_injection: bool,
    pub debug_swap: bool,
    pub prevent_host_ibs: bool,
    pub snp_btb_isolation: bool,
    pub vmpl_isss: bool,
    pub secure_tsc: bool,
    pub vmgexit_param: bool,
    pub pmc_virt: bool,
    pub ibs_virt: bool,
    rsvd: bool,
    pub vmsa_reg_prot: bool,
    pub smt_prot: bool,
    pub secure_avic: bool,
    #[bits(47)]
    _unused: u64,
}

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct SevVirtualInterruptControl {
    pub tpr: u8,
    pub irq: bool,
    pub gif: bool,
    pub intr_shadow: bool,
    #[bits(5)]
    _rsvd1: u64,
    #[bits(4)]
    pub priority: u64,
    pub ignore_tpr: bool,
    #[bits(11)]
    _rsvd2: u64,
    pub vector: u8,
    #[bits(23)]
    _rsvd3: u64,
    pub guest_busy: bool,
}

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct SevRmpAdjust {
    pub target_vmpl: u8,
    pub enable_read: bool,
    pub enable_write: bool,
    pub enable_user_execute: bool,
    pub enable_kernel_execute: bool,
    #[bits(4)]
    _rsvd1: u64,
    pub vmsa: bool,
    #[bits(47)]
    _rsvd2: u64,
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct SevIoAccessInfo {
    pub read_access: bool,
    #[bits(1)]
    reserved1: u32,
    pub string_access: bool,
    pub rep_access: bool,
    pub access_size8: bool,
    pub access_size16: bool,
    pub access_size32: bool,
    pub address_size8: bool,
    pub address_size16: bool,
    pub address_size32: bool,
    #[bits(3)]
    pub effective_segment: u32,
    #[bits(3)]
    rsvd2: u32,
    pub port: u16,
}

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct SevNpfInfo {
    pub present: bool,
    pub read_write: bool,
    pub user: bool,
    pub reserved_bit_set: bool,
    pub fetch: bool,
    #[bits(1)]
    rsvd5: u64,
    pub shadow_stack: bool,
    #[bits(24)]
    rsvd7_31: u64,
    pub rmp_failure: bool,
    pub caused_by_gpa_access: bool,
    pub caused_by_page_table_access: bool,
    pub encrypted_access: bool,
    pub rmp_size_mismatch: bool,
    pub vmpl_violation: bool,
    pub npt_supervisor_shadow_stack: bool,
    #[bits(26)]
    rsvd38_63: u64,
}

/// SEV VMSA structure representing CPU state
#[repr(C)]
#[derive(Debug, Clone, IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct SevVmsa {
    // Selector Info
    pub es: SevSelector,
    pub cs: SevSelector,
    pub ss: SevSelector,
    pub ds: SevSelector,
    pub fs: SevSelector,
    pub gs: SevSelector,

    // Descriptor Table Info
    pub gdtr: SevSelector,
    pub ldtr: SevSelector,
    pub idtr: SevSelector,
    pub tr: SevSelector,

    // CET
    pub pl0_ssp: u64,
    pub pl1_ssp: u64,
    pub pl2_ssp: u64,
    pub pl3_ssp: u64,
    pub u_cet: u64,

    // Reserved, MBZ
    pub vmsa_reserved1: [u8; 2],

    // Virtual Machine Privilege Level
    pub vmpl: u8,

    // CPL
    pub cpl: u8,

    // Reserved, MBZ
    pub vmsa_reserved2: u32,

    // EFER
    pub efer: u64,

    // Reserved, MBZ
    pub vmsa_reserved3: [u32; 26],

    // XSS (offset 0x140)
    pub xss: u64,

    // Control registers
    pub cr4: u64,
    pub cr3: u64,
    pub cr0: u64,

    // Debug registers
    pub dr7: u64,
    pub dr6: u64,

    // RFLAGS
    pub rflags: u64,

    // RIP
    pub rip: u64,

    // Additional saved debug registers
    pub dr0: u64,
    pub dr1: u64,
    pub dr2: u64,
    pub dr3: u64,

    // Debug register address masks
    pub dr0_addr_mask: u64,
    pub dr1_addr_mask: u64,
    pub dr2_addr_mask: u64,
    pub dr3_addr_mask: u64,

    // Reserved, MBZ
    pub vmsa_reserved4: [u64; 3],

    // RSP
    pub rsp: u64,

    // CET
    pub s_cet: u64,
    pub ssp: u64,
    pub interrupt_ssp_table_addr: u64,

    // RAX
    pub rax: u64,

    // SYSCALL config registers
    pub star: u64,
    pub lstar: u64,
    pub cstar: u64,
    pub sfmask: u64,

    // KernelGsBase
    pub kernel_gs_base: u64,

    // SYSENTER config registers
    pub sysenter_cs: u64,
    pub sysenter_esp: u64,
    pub sysenter_eip: u64,

    // CR2
    pub cr2: u64,

    // Reserved, MBZ
    pub vmsa_reserved5: [u64; 4],

    // PAT
    pub pat: u64,

    // LBR MSRs
    pub dbgctl: u64,
    pub last_branch_from_ip: u64,
    pub last_branch_to_ip: u64,
    pub last_excp_from_ip: u64,
    pub last_excp_to_ip: u64,

    // Reserved, MBZ
    pub vmsa_reserved6: [u64; 9],

    // Speculation control MSR
    pub spec_ctrl: u64,

    // PKRU
    pub pkru: u32,

    // TSC_AUX
    pub tsc_aux: u32,

    // Reserved, MBZ
    pub vmsa_reserved7: [u32; 4],

    pub register_protection_nonce: u64,

    // GPRs
    pub rcx: u64,
    pub rdx: u64,
    pub rbx: u64,
    pub vmsa_reserved8: u64, // MBZ
    pub rbp: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,

    // Reserved, MBZ
    pub vmsa_reserved9: [u64; 2],

    // Exit information following an automatic #VMEXIT
    pub exit_info1: u64,
    pub exit_info2: u64,
    pub exit_int_info: u64,

    // Software scratch register
    pub next_rip: u64,

    // SEV feature information
    pub sev_features: SevFeatures,

    // Virtual interrupt control
    pub v_intr_cntrl: SevVirtualInterruptControl,

    // Guest exiting error code
    pub guest_error_code: u64,

    // Virtual top of memory
    pub virtual_tom: u64,

    // TLB control.  Writing a zero to PCPU_ID will force a full TLB
    // invalidation upon the next entry.
    pub tlb_id: u64,
    pub pcpu_id: u64,

    // Event injection
    pub event_inject: SevEventInjectInfo,

    // XCR0
    pub xcr0: u64,

    // X87 state save valid bitmap
    pub xsave_valid_bitmap: [u8; 16],

    // X87 save state
    pub x87dp: u64,
    pub mxcsr: u32,
    pub x87_ftw: u16,
    pub x87_fsw: u16,
    pub x87_fcw: u16,
    pub x87_op: u16,
    pub x87_ds: u16,
    pub x87_cs: u16,
    pub x87_rip: u64,

    // NOTE: Should be 80 bytes. Making it 10 u64 because no code uses it on a
    // byte-level yet.
    pub x87_registers: [u64; 10],

    // XMM registers
    pub xmm_registers: [SevXmmRegister; 16],

    // YMM high registers
    pub ymm_registers: [SevXmmRegister; 16],
}

// Info codes for the GHCB MSR protocol.
open_enum::open_enum! {
    pub enum GhcbInfo: u64 {
        NORMAL = 0x000,
        SEV_INFO_RESPONSE = 0x001,
        SEV_INFO_REQUEST = 0x002,
        AP_JUMP_TABLE = 0x003,
        CPUID_REQUEST = 0x004,
        CPUID_RESPONSE = 0x005,
        PREFERRED_REQUEST = 0x010,
        PREFERRED_RESPONSE = 0x011,
        REGISTER_REQUEST = 0x012,
        REGISTER_RESPONSE = 0x013,
        PAGE_STATE_CHANGE = 0x014,
        PAGE_STATE_UPDATED = 0x015,
        HYP_FEATURE_REQUEST = 0x080,
        HYP_FEATURE_RESPONSE = 0x081,
        SPECIAL_HYPERCALL = 0xF00,
        SPECIAL_FAST_CALL = 0xF01,
        HYPERCALL_OUTPUT = 0xF02,
        SPECIAL_DBGPRINT = 0xF03,
        SHUTDOWN_REQUEST = 0x100,
    }
}

pub const GHCB_DATA_PAGE_STATE_PRIVATE: u64 = 0x001;
pub const GHCB_DATA_PAGE_STATE_SHARED: u64 = 0x002;
pub const GHCB_DATA_PAGE_STATE_PSMASH: u64 = 0x003;
pub const GHCB_DATA_PAGE_STATE_UNSMASH: u64 = 0x004;
pub const GHCB_DATA_PAGE_STATE_MASK: u64 = 0x00F;
pub const GHCB_DATA_PAGE_STATE_LARGE_PAGE: u64 = 0x010;

open_enum::open_enum! {
    pub enum GhcbUsage: u32 {
        BASE = 0,
        HYPERCALL = 1,
        VTL_RETURN = 2,
    }
}

/// Struct representing GHCB hypercall parameters. These are located at the GHCB
/// page starting at [`GHCB_PAGE_HYPERCALL_PARAMETERS_OFFSET`].
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GhcbHypercallParameters {
    pub output_gpa: u64,
    pub input_control: u64,
}

pub const GHCB_PAGE_HYPERCALL_PARAMETERS_OFFSET: usize = 4072;
pub const GHCB_PAGE_HYPERCALL_OUTPUT_OFFSET: usize = 4080;

// Exit Codes.
open_enum::open_enum! {
    pub enum SevExitCode: u64 {
        CR0_READ = 0,
        CR1_READ = 1,
        CR2_READ = 2,
        CR3_READ = 3,
        CR4_READ = 4,
        CR5_READ = 5,
        CR6_READ = 6,
        CR7_READ = 7,
        CR8_READ = 8,
        CR9_READ = 9,
        CR10_READ = 10,
        CR11_READ = 11,
        CR12_READ = 12,
        CR13_READ = 13,
        CR14_READ = 14,
        CR15_READ = 15,
        CR0_WRITE = 16,
        CR1_WRITE = 17,
        CR2_WRITE = 18,
        CR3_WRITE = 19,
        CR4_WRITE = 20,
        CR5_WRITE = 21,
        CR6_WRITE = 22,
        CR7_WRITE = 23,
        CR8_WRITE = 24,
        CR9_WRITE = 25,
        CR10_WRITE = 26,
        CR11_WRITE = 27,
        CR12_WRITE = 28,
        CR13_WRITE = 29,
        CR14_WRITE = 30,
        CR15_WRITE = 31,
        DR0_READ = 32,
        DR1_READ = 33,
        DR2_READ = 34,
        DR3_READ = 35,
        DR4_READ = 36,
        DR5_READ = 37,
        DR6_READ = 38,
        DR7_READ = 39,
        DR8_READ = 40,
        DR9_READ = 41,
        DR10_READ = 42,
        DR11_READ = 43,
        DR12_READ = 44,
        DR13_READ = 45,
        DR14_READ = 46,
        DR15_READ = 47,
        DR0_WRITE = 48,
        DR1_WRITE = 49,
        DR2_WRITE = 50,
        DR3_WRITE = 51,
        DR4_WRITE = 52,
        DR5_WRITE = 53,
        DR6_WRITE = 54,
        DR7_WRITE = 55,
        DR8_WRITE = 56,
        DR9_WRITE = 57,
        DR10_WRITE = 58,
        DR11_WRITE = 59,
        DR12_WRITE = 60,
        DR13_WRITE = 61,
        DR14_WRITE = 62,
        DR15_WRITE = 63,
        EXCP0 = 64,
        EXCP_DB = 65,
        EXCP2 = 66,
        EXCP3 = 67,
        EXCP4 = 68,
        EXCP5 = 69,
        EXCP6 = 70,
        EXCP7 = 71,
        EXCP8 = 72,
        EXCP9 = 73,
        EXCP10 = 74,
        EXCP11 = 75,
        EXCP12 = 76,
        EXCP13 = 77,
        EXCP14 = 78,
        EXCP15 = 79,
        EXCP16 = 80,
        EXCP17 = 81,
        EXCP18 = 82,
        EXCP19 = 83,
        EXCP20 = 84,
        EXCP21 = 85,
        EXCP22 = 86,
        EXCP23 = 87,
        EXCP24 = 88,
        EXCP25 = 89,
        EXCP26 = 90,
        EXCP27 = 91,
        EXCP28 = 92,
        EXCP29 = 93,
        EXCP30 = 94,
        EXCP31 = 95,
        INTR = 96,
        NMI = 97,
        SMI = 98,
        INIT = 99,
        VINTR = 100,
        CR0_SEL_WRITE = 101,
        IDTR_READ = 102,
        GDTR_READ = 103,
        LDTR_READ = 104,
        TR_READ = 105,
        IDTR_WRITE = 106,
        GDTR_WRITE = 107,
        LDTR_WRITE = 108,
        TR_WRITE = 109,
        RDTSC = 110,
        RDPMC = 111,
        PUSHF = 112,
        POPF = 113,
        CPUID = 114,
        RSM = 115,
        IRET = 116,
        SWINT = 117,
        INVD = 118,
        PAUSE = 119,
        HLT = 120,
        INVLPG = 121,
        INVLPGA = 122,
        IOIO = 123,
        MSR = 124,
        TASK_SWITCH = 125,
        FERR_FREEZE = 126,
        SHUTDOWN = 127,
        VMRUN = 128,
        VMMCALL = 129,
        VMLOAD = 130,
        VMSAVE = 131,
        STGI = 132,
        CLGI = 133,
        SKINIT = 134,
        RDTSCP = 135,
        ICEBP = 136,
        WBINVD = 137,
        MONITOR = 138,
        MWAIT = 139,
        MWAIT_CONDITIONAL = 140,
        XSETBV = 141,
        INVLPGB = 160,
        ILLEGAL_INVLPGB = 161,
        NPF = 1024,
        AVIC_INCOMPLETE_IPI = 1025,
        AVIC_NOACCEL = 1026,
        VMGEXIT = 1027,
        PAGE_NOT_VALIDATED = 1028,
        SNP_GUEST_REQUEST = 0x80000011,
        SNP_EXTENDED_GUEST_REQUEST = 0x80000012,
        HV_DOORBELL_PAGE = 0x80000014,
        INVALID_VMCB = 0xffff_ffff_ffff_ffff,
    }
}

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct GhcbMsr {
    #[bits(12)]
    pub info: u64,
    #[bits(40)]
    pub pfn: u64,
    #[bits(12)]
    pub extra_data: u64,
}

/// PSP data structures.
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
pub struct HvPspCpuidLeaf {
    pub eax_in: u32,
    pub ecx_in: u32,
    pub xfem_in: u64,
    pub xss_in: u64,
    pub eax_out: u32,
    pub ebx_out: u32,
    pub ecx_out: u32,
    pub edx_out: u32,
    pub reserved_z: u64,
}

pub const HV_PSP_CPUID_LEAF_COUNT_MAX: usize = 64;

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
pub struct HvPspCpuidPage {
    pub count: u32,
    pub reserved_z1: u32,
    pub reserved_z2: u64,
    pub cpuid_leaf_info: [HvPspCpuidLeaf; HV_PSP_CPUID_LEAF_COUNT_MAX],
    pub reserved_z3: [u64; 126],
}

/// Structure describing the pages being read during SNP ID block measurement.
/// Each structure is hashed with the previous structures digest to create a final
/// measurement
#[repr(C)]
#[derive(Debug, Clone, Copy, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SnpPageInfo {
    /// Set to the value of the previous page's launch digest
    pub digest_current: [u8; 48],
    /// Hash of page contents, if measured
    pub contents: [u8; 48],
    /// Size of the SnpPageInfo struct
    pub length: u16,
    /// type of page being measured, described by [`SnpPageType`]
    pub page_type: SnpPageType,
    /// imi_page_bit must match IMI_PAGE flag
    pub imi_page_bit: u8,
    /// All lower VMPL permissions are denied for SNP
    pub lower_vmpl_permissions: u32,
    /// The guest physical address at which this page data should be loaded; it
    /// must be aligned to a page size boundary.
    pub gpa: u64,
}

open_enum::open_enum! {
    /// The type of page described by [`SnpPageInfo`]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum SnpPageType: u8 {
        /// Reserved
        RESERVED = 0x0,
        /// Normal data page
        NORMAL = 0x1,
        /// VMSA page
        VMSA = 0x2,
        /// Zero page
        ZERO = 0x3,
        /// Page encrypted, but not measured
        UNMEASURED = 0x4,
        /// Page storing guest secrets
        SECRETS = 0x5,
        /// Page to provide CPUID function values
        CPUID = 0x6,
    }
}

/// Structure containing the completed SNP measurement of the IGVM file.
/// The signature of the hash of this struct is the id_key_signature for
/// `igvm_defs::IGVM_VHS_SNP_ID_BLOCK`.
#[repr(C)]
#[derive(Debug, Clone, Copy, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SnpPspIdBlock {
    /// completed launch digest of IGVM file
    pub ld: [u8; 48],
    /// family id of the guest
    pub family_id: [u8; 16],
    /// image id of the guest
    pub image_id: [u8; 16],
    /// Version of the ID block format, must be 0x1
    pub version: u32,
    /// Software version of the guest
    pub guest_svn: u32,
    /// SNP Policy of the guest
    pub policy: u64,
}

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct SevStatusMsr {
    pub sev_enabled: bool,
    pub es_enabled: bool,
    pub snp_enabled: bool,
    pub vtom: bool,
    pub reflect_vc: bool,
    pub restrict_injection: bool,
    pub alternate_injection: bool,
    pub debug_swap: bool,
    pub prevent_host_ibs: bool,
    pub snp_btb_isolation: bool,
    pub _rsvd1: bool,
    pub secure_tsc: bool,
    pub _rsvd2: bool,
    pub _rsvd3: bool,
    pub _rsvd4: bool,
    pub _rsvd5: bool,
    pub vmsa_reg_prot: bool,
    #[bits(47)]
    _unused: u64,
}

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct SevInvlpgbRax {
    pub va_valid: bool,
    pub pcid_valid: bool,
    pub asid_valid: bool,
    pub global: bool,
    pub final_only: bool,
    pub nested: bool,
    #[bits(6)]
    reserved: u64,
    #[bits(52)]
    pub virtual_page_number: u64,
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct SevInvlpgbEdx {
    #[bits(16)]
    pub asid: u64,
    #[bits(12)]
    pub pcid: u64,
    #[bits(4)]
    reserved: u32,
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct SevInvlpgbEcx {
    #[bits(16)]
    pub additional_count: u64,
    #[bits(15)]
    reserved: u64,
    pub large_page: bool,
}
