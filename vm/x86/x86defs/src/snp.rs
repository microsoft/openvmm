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

/// Value for the `msg_version` member in [`SNP_GUEST_REQ_MSG_VERSION`].
/// Use 1 for now.
pub const SNP_GUEST_REQ_MSG_VERSION: u32 = 1;

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
    #[bits(4)]
    _reserved: u64,
    pub ibpb_on_entry: bool,
    #[bits(42)]
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
    #[bits(1)]
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
    pub is_write: bool,
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
    #[bits(25)]
    rsvd38_63: u64,
    pub not_restartable: bool,
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
        CR0_READ = 0x0,
        CR1_READ = 0x1,
        CR2_READ = 0x2,
        CR3_READ = 0x3,
        CR4_READ = 0x4,
        CR5_READ = 0x5,
        CR6_READ = 0x6,
        CR7_READ = 0x7,
        CR8_READ = 0x8,
        CR9_READ = 0x9,
        CR10_READ = 0xa,
        CR11_READ = 0xb,
        CR12_READ = 0xc,
        CR13_READ = 0xd,
        CR14_READ = 0xe,
        CR15_READ = 0xf,
        CR0_WRITE = 0x10,
        CR1_WRITE = 0x11,
        CR2_WRITE = 0x12,
        CR3_WRITE = 0x13,
        CR4_WRITE = 0x14,
        CR5_WRITE = 0x15,
        CR6_WRITE = 0x16,
        CR7_WRITE = 0x17,
        CR8_WRITE = 0x18,
        CR9_WRITE = 0x19,
        CR10_WRITE = 0x1a,
        CR11_WRITE = 0x1b,
        CR12_WRITE = 0x1c,
        CR13_WRITE = 0x1d,
        CR14_WRITE = 0x1e,
        CR15_WRITE = 0x1f,
        DR0_READ = 0x20,
        DR1_READ = 0x21,
        DR2_READ = 0x22,
        DR3_READ = 0x23,
        DR4_READ = 0x24,
        DR5_READ = 0x25,
        DR6_READ = 0x26,
        DR7_READ = 0x27,
        DR8_READ = 0x28,
        DR9_READ = 0x29,
        DR10_READ = 0x2a,
        DR11_READ = 0x2b,
        DR12_READ = 0x2c,
        DR13_READ = 0x2d,
        DR14_READ = 0x2e,
        DR15_READ = 0x2f,
        DR0_WRITE = 0x30,
        DR1_WRITE = 0x31,
        DR2_WRITE = 0x32,
        DR3_WRITE = 0x33,
        DR4_WRITE = 0x34,
        DR5_WRITE = 0x35,
        DR6_WRITE = 0x36,
        DR7_WRITE = 0x37,
        DR8_WRITE = 0x38,
        DR9_WRITE = 0x39,
        DR10_WRITE = 0x3a,
        DR11_WRITE = 0x3b,
        DR12_WRITE = 0x3c,
        DR13_WRITE = 0x3d,
        DR14_WRITE = 0x3e,
        DR15_WRITE = 0x3f,
        EXCP0 = 0x40,
        EXCP_DB = 0x41,
        EXCP2 = 0x42,
        EXCP3 = 0x43,
        EXCP4 = 0x44,
        EXCP5 = 0x45,
        EXCP6 = 0x46,
        EXCP7 = 0x47,
        EXCP8 = 0x48,
        EXCP9 = 0x49,
        EXCP10 = 0x4a,
        EXCP11 = 0x4b,
        EXCP12 = 0x4c,
        EXCP13 = 0x4d,
        EXCP14 = 0x4e,
        EXCP15 = 0x4f,
        EXCP16 = 0x50,
        EXCP17 = 0x51,
        EXCP18 = 0x52,
        EXCP19 = 0x53,
        EXCP20 = 0x54,
        EXCP21 = 0x55,
        EXCP22 = 0x56,
        EXCP23 = 0x57,
        EXCP24 = 0x58,
        EXCP25 = 0x59,
        EXCP26 = 0x5a,
        EXCP27 = 0x5b,
        EXCP28 = 0x5c,
        EXCP29 = 0x5d,
        EXCP30 = 0x5e,
        EXCP31 = 0x5f,
        INTR = 0x60,
        NMI = 0x61,
        SMI = 0x62,
        INIT = 0x63,
        VINTR = 0x64,
        CR0_SEL_WRITE = 0x65,
        IDTR_READ = 0x66,
        GDTR_READ = 0x67,
        LDTR_READ = 0x68,
        TR_READ = 0x69,
        IDTR_WRITE = 0x6a,
        GDTR_WRITE = 0x6b,
        LDTR_WRITE = 0x6c,
        TR_WRITE = 0x6d,
        RDTSC = 0x6e,
        RDPMC = 0x6f,
        PUSHF = 0x70,
        POPF = 0x71,
        CPUID = 0x72,
        RSM = 0x73,
        IRET = 0x74,
        SWINT = 0x75,
        INVD = 0x76,
        PAUSE = 0x77,
        HLT = 0x78,
        INVLPG = 0x79,
        INVLPGA = 0x7a,
        IOIO = 0x7b,
        MSR = 0x7c,
        TASK_SWITCH = 0x7d,
        FERR_FREEZE = 0x7e,
        SHUTDOWN = 0x7f,
        VMRUN = 0x80,
        VMMCALL = 0x81,
        VMLOAD = 0x82,
        VMSAVE = 0x83,
        STGI = 0x84,
        CLGI = 0x85,
        SKINIT = 0x86,
        RDTSCP = 0x87,
        ICEBP = 0x88,
        WBINVD = 0x89,
        MONITOR = 0x8a,
        MWAIT = 0x8b,
        MWAIT_CONDITIONAL = 0x8c,
        XSETBV = 0x8d,
        RDPRU = 0x8e,
        EFER_WRITE_TRAP = 0x8f,
        CR0_WRITE_TRAP = 0x90,
        CR1_WRITE_TRAP = 0x91,
        CR2_WRITE_TRAP = 0x92,
        CR3_WRITE_TRAP = 0x93,
        CR4_WRITE_TRAP = 0x94,
        CR5_WRITE_TRAP = 0x95,
        CR6_WRITE_TRAP = 0x96,
        CR7_WRITE_TRAP = 0x97,
        CR8_WRITE_TRAP = 0x98,
        CR9_WRITE_TRAP = 0x99,
        CR10_WRITE_TRAP = 0x9a,
        CR11_WRITE_TRAP = 0x9b,
        CR12_WRITE_TRAP = 0x9c,
        CR13_WRITE_TRAP = 0x9d,
        CR14_WRITE_TRAP = 0x9e,
        CR15_WRITE_TRAP = 0x9f,
        INVLPGB = 0xa0,
        ILLEGAL_INVLPGB = 0xa1,
        INVPCID = 0xa2,
        BUSLOCK = 0xa5,
        IDLE_HLT = 0xa6,
        NPF = 0x400,
        AVIC_INCOMPLETE_IPI = 0x401,
        AVIC_NOACCEL = 0x402,
        VMGEXIT = 0x403,
        PAGE_NOT_VALIDATED = 0x404,
        NOT_RESTARTABLE = 0x406,

        // SEV-ES software-defined exit codes
        SNP_GUEST_REQUEST = 0x80000011,
        SNP_EXTENDED_GUEST_REQUEST = 0x80000012,
        HV_DOORBELL_PAGE = 0x80000014,

        // SEV-SNP hardware error codes
        INVALID_VMCB = 0xffff_ffff_ffff_ffff,
        VMSA_BUSY = 0xffff_ffff_ffff_fffe,
        IDLE_REQUIRED = 0xffff_ffff_ffff_fffd,
        INVALID_PMC = 0xffff_ffff_ffff_fffc,
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
    #[bits(6)]
    _reserved: u64,
    pub ibpb_on_entry: bool,
    #[bits(40)]
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

#[bitfield(u64)]
pub struct MovCrxDrxInfo {
    #[bits(4)]
    pub gpr_number: u64,
    #[bits(59)]
    pub reserved: u64,
    pub mov_crx: bool,
}

/// Request structure for the `SNP_GET_REPORT` request.
/// See `MSG_REPORT_REQ` in Table 21, "SEV Secure Nested Paging Firmware ABI specification", Revision 1.55.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SnpReportReq {
    /// Guest-provided data to be included in the attestation report.
    pub user_data: [u8; 64],
    /// The VMPL to put in the attestation report. Must be greater than
    /// or equal to the current VMPL and, at most, three.
    pub vmpl: u32,
    /// Reserved
    // TODO SNP: Support VLEK feature if needed
    pub rsvd: [u8; 28],
}

pub const SNP_REPORT_RESP_DATA_SIZE: usize =
    size_of::<u32>() + size_of::<u32>() + 24 + size_of::<SnpReport>();

/// Response structure for the `SNP_GET_REPORT` request.
/// See `MSG_REPORT_RSP` in Table 24, "SEV Secure Nested Paging Firmware ABI specification", Revision 1.55.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SnpReportResp {
    /// The status of key derivation operation.
    /// 0h: Success.
    /// 16h: Invalid parameters.
    /// 27h: Invalid key selection.
    pub status: u32,
    /// Size in bytes of the report.
    pub report_size: u32,
    /// Reserved
    pub _reserved0: [u8; 24],
    /// The attestation report generated by the firmware.
    pub report: SnpReport,
}

/// Size of the [`SnpReport`].
pub const SNP_REPORT_SIZE: usize = 0x4a0;

/// Size of `report_data` member in [`SnpReport`].
pub const SNP_REPORT_DATA_SIZE: usize = 64;

/// Report structure.
/// See `ATTESTATION_REPORT` in Table 22, "SEV Secure Nested Paging Firmware ABI specification", Revision 1.55.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SnpReport {
    /// Version number of this attestation report.
    /// Set to 2h for this specification.
    pub version: u32,
    /// The guest SVN.
    pub guest_svn: u32,
    /// The guest policy.
    pub policy: u64,
    /// The family ID provided at launch.
    pub family: u128,
    /// The image ID provided at launch.
    pub image_id: u128,
    /// The request VMPL for the attestation
    /// report.
    pub vmpl: u32,
    /// The signature algorithm used to sign
    /// this report.
    pub signature_algo: u32,
    /// CurrentTcb.
    pub current_tcb: u64,
    /// Information about the platform.
    pub platform_info: u64,
    /// Flags
    pub flags: u32,
    /// Reserved
    pub _reserved0: u32,
    /// Guest-provided data.
    pub report_data: [u8; SNP_REPORT_DATA_SIZE],
    /// The measurement calculated at
    /// launch.
    pub measurement: [u8; 48],
    /// Data provided by the hypervisor at
    /// launch.
    pub host_data: [u8; 32],
    /// SHA-384 digest of the ID public key
    /// that signed the ID block provided in
    /// SNP_LAUNCH_FINISH.
    pub id_key_digest: [u8; 48],
    /// SHA-384 digest of the Author public
    /// key that certified the ID key, if
    /// provided in SNP_LAUNCH_FINISH.
    pub author_key_digest: [u8; 48],
    /// Report ID of this guest.
    pub report_id: [u8; 32],
    /// Report ID of this guest’s migration
    /// agent
    pub report_id_ma: [u8; 32],
    /// Reported TCB version used to derive
    /// the VCEK that signed this report.
    pub reported_tcb: u64,
    /// Reserved
    pub _reserved1: [u8; 24],
    /// If MaskChipId is set to 0, Identifier
    /// unique to the chip as output by
    /// GET_ID. Otherwise, set to 0h.
    pub chip_id: [u8; 64],
    /// CommittedTcb.
    pub committed_tcb: u64,
    /// The build number of CurrentVersion.
    pub current_build: u8,
    /// The minor number of CurrentVersion.
    pub current_minor: u8,
    /// The major number of CurrentVersion.
    pub current_major: u8,
    /// Reserved
    pub _reserved2: u8,
    /// The build number of CommittedVersion.
    pub committed_build: u8,
    /// The minor version of CommittedVersion.
    pub committed_minor: u8,
    /// The major version of CommittedVersion.
    pub committed_major: u8,
    /// Reserved
    pub _reserved3: u8,
    /// The CurrentTcb at the time the guest
    /// was launched or imported.
    pub launch_tcb: u64,
    /// Reserved
    pub _reserved4: [u8; 168],
    /// Signature of bytes inclusive of this report.
    pub signature: [u8; 512],
}

static_assertions::const_assert_eq!(SNP_REPORT_SIZE, size_of::<SnpReport>());

/// Request structure for the `SNP_GET_DERIVED_KEY` request.
/// See `MSG_KEY_REQ` in Table 18, "SEV Secure Nested Paging Firmware ABI specification", Revision 1.55.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SnpDerivedKeyReq {
    /// Selects the root key from which to derive the key.
    /// 0 indicates VCEK
    /// 1 indicates VMRK
    // TODO: Support VLEK feature if needed
    pub root_key_select: u32,
    /// Reserved
    pub rsvd: u32,
    /// Bitmask indicating which data will be mixed into the
    /// derived key.
    pub guest_field_select: u64,
    /// The VMPL to mix into the derived key. Must be greater
    /// than or equal to the current VMPL.
    pub vmpl: u32,
    /// The guest SVN to mix into the key. Must not exceed the
    /// guest SVN provided at launch in the ID block.
    pub guest_svn: u32,
    /// The TCB version to mix into the derived key. Must not
    /// exceed CommittedTcb.
    pub tcb_version: u64,
}

/// Indicate which guest-selectable fields will be mixed into the key.
/// See `GUEST_FIELD_SELECT` in Table 19, "SEV Secure Nested Paging Firmware ABI specification", Revision 1.55.
#[bitfield(u64)]
pub struct GuestFieldSelect {
    /// Indicate that the guest policy will be mixed into the key.
    pub guest_policy: bool,
    /// Indicate that the image ID of the guest will be mixed into the key.
    pub image_id: bool,
    /// Indicate the family ID of the guest will be mixed into the key.
    pub family_id: bool,
    /// Indicate the measurement of the guest during launch will be mixed into the key.
    pub measurement: bool,
    /// Indicate that the guest-provided SVN will be mixed into the key.
    pub guest_svn: bool,
    /// Indicate that the guest-provided TCB_VERSION will be mixed into the key.
    pub tcb_version: bool,
    /// Reserved
    #[bits(58)]
    pub _reserved: u64,
}

/// See `DERIVED_KEY` in Table 20, "SEV Secure Nested Paging Firmware ABI specification", Revision 1.55.
pub const SNP_DERIVED_KEY_SIZE: usize = 32;

/// Response structure for the `SNP_GET_DERIVED_KEY` request.
/// See `MSG_KEY_RSP` in Table 20, "SEV Secure Nested Paging Firmware ABI specification", Revision 1.55.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SnpDerivedKeyResp {
    /// The status of key derivation operation.
    /// 0h: Success.
    /// 16h: Invalid parameters.
    /// 27h: Invalid key selection.
    pub status: u32,
    /// Reserved
    pub _reserved: [u8; 28],
    /// The requested derived key.
    pub derived_key: [u8; SNP_DERIVED_KEY_SIZE],
}

static_assertions::const_assert_eq!(
    // The size of the response data defined by the SNP specification.
    64,
    size_of::<SnpDerivedKeyResp>()
);
