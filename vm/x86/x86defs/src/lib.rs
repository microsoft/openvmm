// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Definitions relating to the x86 architecture, including the core CPU and
//! its interrupt controller (APIC).

#![no_std]
#![forbid(unsafe_code)]

pub mod apic;
pub mod cpuid;
pub mod msi;
pub mod snp;
pub mod tdx;
pub mod vmx;
pub mod xsave;

use bitfield_struct::bitfield;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

pub const X64_CR0_PE: u64 = 0x0000000000000001; // protection enable
pub const X64_CR0_MP: u64 = 0x0000000000000002; // math present
pub const X64_CR0_EM: u64 = 0x0000000000000004; // emulate math coprocessor
pub const X64_CR0_TS: u64 = 0x0000000000000008; // task switched
pub const X64_CR0_ET: u64 = 0x0000000000000010; // extension type (80387)
pub const X64_CR0_NE: u64 = 0x0000000000000020; // numeric error
pub const X64_CR0_WP: u64 = 0x0000000000010000; // write protect
pub const X64_CR0_AM: u64 = 0x0000000000040000; // alignment mask
pub const X64_CR0_NW: u64 = 0x0000000020000000; // not write-through
pub const X64_CR0_CD: u64 = 0x0000000040000000; // cache disable
pub const X64_CR0_PG: u64 = 0x0000000080000000; // paging

pub const X64_CR4_VME: u64 = 0x0000000000000001; // Virtual 8086 mode extensions
pub const X64_CR4_PVI: u64 = 0x0000000000000002; // Protected mode virtual interrupts
pub const X64_CR4_TSD: u64 = 0x0000000000000004; // Time stamp disable
pub const X64_CR4_DE: u64 = 0x0000000000000008; // Debugging extensions
pub const X64_CR4_PSE: u64 = 0x0000000000000010; // Page size extensions
pub const X64_CR4_PAE: u64 = 0x0000000000000020; // Physical address extensions
pub const X64_CR4_MCE: u64 = 0x0000000000000040; // Machine check enable
pub const X64_CR4_PGE: u64 = 0x0000000000000080; // Page global enable
pub const X64_CR4_PCE: u64 = 0x0000000000000100; // Performance Counter Enable
pub const X64_CR4_FXSR: u64 = 0x0000000000000200; // FXSR used by OS
pub const X64_CR4_XMMEXCPT: u64 = 0x0000000000000400; // XMMI used by OS
pub const X64_CR4_UMIP: u64 = 0x0000000000000800; // UMIP used by OS
pub const X64_CR4_LA57: u64 = 0x0000000000001000; // 5-level paging enabled
pub const X64_CR4_VMXE: u64 = 0x0000000000002000; // VMX enabled
pub const X64_CR4_RWFSGS: u64 = 0x0000000000010000; // RDWRFSGS enabled by OS
pub const X64_CR4_PCIDE: u64 = 0x0000000000020000; // PCID enabled by OS
pub const X64_CR4_OSXSAVE: u64 = 0x0000000000040000; // XSAVE enabled by OS
pub const X64_CR4_SMEP: u64 = 0x0000000000100000; // Supervisor Mode Execution Protection
pub const X64_CR4_SMAP: u64 = 0x0000000000200000; // Supervisor Mode Access Protection
pub const X64_CR4_CET: u64 = 0x0000000000800000; // CET enabled by OS

pub const X64_EFER_SCE: u64 = 0x0000000000000001; // Syscall Enable
pub const X64_EFER_LME: u64 = 0x0000000000000100; // Long Mode Enabled
pub const X64_EFER_LMA: u64 = 0x0000000000000400; // Long Mode Active
pub const X64_EFER_NXE: u64 = 0x0000000000000800; // No-execute Enable
pub const X64_EFER_SVME: u64 = 0x0000000000001000; // SVM enable
pub const X64_EFER_FFXSR: u64 = 0x0000000000004000; // Fast save/restore enabled

pub const X86X_MSR_DEFAULT_PAT: u64 = 0x0007040600070406;
pub const X64_EMPTY_DR7: u64 = 0x0000000000000400;

pub const USER_MODE_DPL: u8 = 3;

pub const X64_DEFAULT_CODE_SEGMENT_ATTRIBUTES: SegmentAttributes = SegmentAttributes::new()
    .with_granularity(true)
    .with_long(true)
    .with_present(true)
    .with_non_system_segment(true)
    .with_segment_type(0xb);
pub const X64_DEFAULT_DATA_SEGMENT_ATTRIBUTES: SegmentAttributes = SegmentAttributes::new()
    .with_granularity(true)
    .with_default(true)
    .with_present(true)
    .with_non_system_segment(true)
    .with_segment_type(0x3);
pub const X64_BUSY_TSS_SEGMENT_ATTRIBUTES: SegmentAttributes = SegmentAttributes::new()
    .with_present(true)
    .with_segment_type(0xb);

#[bitfield(u16)]
#[derive(PartialEq)]
pub struct SegmentAttributes {
    #[bits(4)]
    pub segment_type: u8,
    pub non_system_segment: bool,
    #[bits(2)]
    pub descriptor_privilege_level: u8,
    pub present: bool,
    #[bits(4)]
    _reserved: u8,
    pub available: bool,
    pub long: bool,
    pub default: bool,
    pub granularity: bool,
}

impl SegmentAttributes {
    pub const fn as_bits(&self) -> u16 {
        self.0
    }
}

#[cfg(feature = "arbitrary")]
impl<'a> arbitrary::Arbitrary<'a> for SegmentAttributes {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let x: u16 = u.arbitrary()?;
        Ok(x.into())
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct SegmentRegister {
    pub base: u64,
    pub limit: u32,
    pub selector: u16,
    pub attributes: SegmentAttributes,
}

#[cfg(feature = "arbitrary")]
impl<'a> arbitrary::Arbitrary<'a> for SegmentRegister {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        Ok(SegmentRegister {
            base: u.arbitrary()?,
            limit: u.arbitrary()?,
            selector: u.arbitrary()?,
            attributes: u.arbitrary()?,
        })
    }
}

/// Values for `X86X_IA32_MSR_MISC_ENABLE` MSR.
///
/// Many of these fields are undocumented or underdocumented and do not always
/// have the same meaning across different CPU models. However, this MSR must
/// have appropriate values for Linux to successfully boot.
#[bitfield(u64)]
pub struct MiscEnable {
    pub fast_string: bool,
    pub tcc: bool,
    pub x87_compat: bool,
    pub tm1: bool,
    pub split_lock_disable: bool,
    _reserved5: bool,
    pub l3cache_disable: bool,
    pub emon: bool,
    pub suppress_lock: bool,
    pub prefetch_disable: bool,
    pub ferr: bool,
    pub bts_unavailable: bool,
    pub pebs_unavailable: bool,
    pub tm2: bool,
    _reserved14: bool,
    _reserved15: bool,
    pub enhanced_speedstep: bool,
    _reserved17: bool,
    pub mwait: bool,
    pub adj_prefetch_disable: bool,
    pub enable_speedstep_lock: bool,
    _reserved21: bool,
    pub limit_cpuid: bool,
    pub xtpr_disable: bool,
    pub l1d_context: bool,
    #[bits(39)]
    _reserved: u64,
}

pub const X86X_MSR_TSC: u32 = 0x10;
pub const X86X_IA32_MSR_PLATFORM_ID: u32 = 0x17;
pub const X86X_MSR_APIC_BASE: u32 = 0x1b;
pub const X86X_MSR_EBL_CR_POWERON: u32 = 0x2a;
pub const X86X_IA32_MSR_SMI_COUNT: u32 = 0x34;
pub const X86X_IA32_MSR_FEATURE_CONTROL: u32 = 0x3a;
pub const X86X_MSR_PPIN_CTL: u32 = 0x4e;
pub const X86X_MSR_MC_UPDATE_PATCH_LEVEL: u32 = 0x8b;
pub const X86X_MSR_PLATFORM_INFO: u32 = 0xce;
pub const X86X_MSR_UMWAIT_CONTROL: u32 = 0xe1;
pub const X86X_MSR_MTRR_CAP: u32 = 0xfe;
pub const X86X_MSR_MISC_FEATURE_ENABLES: u32 = 0x140;
pub const X86X_MSR_SYSENTER_CS: u32 = 0x174;
pub const X86X_MSR_SYSENTER_ESP: u32 = 0x175;
pub const X86X_MSR_SYSENTER_EIP: u32 = 0x176;
pub const X86X_MSR_MCG_CAP: u32 = 0x179;
pub const X86X_MSR_MCG_STATUS: u32 = 0x17a;
pub const X86X_IA32_MSR_MISC_ENABLE: u32 = 0x1a0;
pub const X86X_MSR_MTRR_PHYSBASE0: u32 = 0x200;
pub const X86X_MSR_MTRR_FIX64K_00000: u32 = 0x0250;
pub const X86X_MSR_MTRR_FIX16K_80000: u32 = 0x0258;
pub const X86X_MSR_MTRR_FIX16K_A0000: u32 = 0x0259;
pub const X86X_MSR_MTRR_FIX4K_C0000: u32 = 0x0268;
pub const X86X_MSR_MTRR_FIX4K_C8000: u32 = 0x0269;
pub const X86X_MSR_MTRR_FIX4K_D0000: u32 = 0x026A;
pub const X86X_MSR_MTRR_FIX4K_D8000: u32 = 0x026B;
pub const X86X_MSR_MTRR_FIX4K_E0000: u32 = 0x026C;
pub const X86X_MSR_MTRR_FIX4K_E8000: u32 = 0x026D;
pub const X86X_MSR_MTRR_FIX4K_F0000: u32 = 0x026E;
pub const X86X_MSR_MTRR_FIX4K_F8000: u32 = 0x026F;
pub const X86X_MSR_CR_PAT: u32 = 0x277;
pub const X86X_MSR_MTRR_DEF_TYPE: u32 = 0x2ff;

pub const X86X_MSR_XSS: u32 = 0xda0;

pub const X86X_IA32_MSR_RAPL_POWER_UNIT: u32 = 0x606;
pub const X86X_IA32_MSR_PKG_ENERGY_STATUS: u32 = 0x611;
pub const X86X_IA32_MSR_DRAM_ENERGY_STATUS: u32 = 0x619;
pub const X86X_IA32_MSR_PP0_ENERGY_STATUS: u32 = 0x639;

pub const X86X_MSR_U_CET: u32 = 0x6a0;
pub const X86X_MSR_S_CET: u32 = 0x6a2;
pub const X86X_MSR_PL0_SSP: u32 = 0x6a4;
pub const X86X_MSR_PL1_SSP: u32 = 0x6a5;
pub const X86X_MSR_PL2_SSP: u32 = 0x6a6;
pub const X86X_MSR_PL3_SSP: u32 = 0x6a7;
pub const X86X_MSR_INTERRUPT_SSP_TABLE_ADDR: u32 = 0x6a8;

pub const X86X_MSR_STAR: u32 = 0xC0000081;
pub const X86X_MSR_LSTAR: u32 = 0xC0000082;
pub const X86X_MSR_CSTAR: u32 = 0xC0000083;
pub const X86X_MSR_SFMASK: u32 = 0xC0000084;

pub const X86X_MSR_EFER: u32 = 0xC0000080;
pub const X64_MSR_FS_BASE: u32 = 0xC0000100;
pub const X64_MSR_GS_BASE: u32 = 0xC0000101;
pub const X64_MSR_KERNEL_GS_BASE: u32 = 0xC0000102;

pub const X86X_MSR_TSC_AUX: u32 = 0xC0000103;

pub const X86X_MSR_SPEC_CTRL: u32 = 0x48;
pub const X86X_IA32_MSR_XFD: u32 = 0x1C4;
pub const X86X_IA32_MSR_XFD_ERR: u32 = 0x1C5;

pub const X86X_AMD_MSR_PERF_EVT_SEL0: u32 = 0xC0010000;
pub const X86X_AMD_MSR_PERF_EVT_SEL1: u32 = 0xC0010001;
pub const X86X_AMD_MSR_PERF_EVT_SEL2: u32 = 0xC0010002;
pub const X86X_AMD_MSR_PERF_EVT_SEL3: u32 = 0xC0010003;
pub const X86X_AMD_MSR_PERF_CTR0: u32 = 0xC0010004;
pub const X86X_AMD_MSR_PERF_CTR1: u32 = 0xC0010005;
pub const X86X_AMD_MSR_PERF_CTR2: u32 = 0xC0010006;
pub const X86X_AMD_MSR_PERF_CTR3: u32 = 0xC0010007;
pub const X86X_AMD_MSR_SYSCFG: u32 = 0xC0010010;
pub const X86X_AMD_MSR_HW_CFG: u32 = 0xC0010015;
pub const X86X_AMD_MSR_NB_CFG: u32 = 0xC001001F;
pub const X86X_AMD_MSR_VM_CR: u32 = 0xC0010114;
pub const X86X_AMD_MSR_GHCB: u32 = 0xC0010130;
pub const X86X_AMD_MSR_SEV: u32 = 0xC0010131;
pub const X86X_AMD_MSR_OSVW_ID_LENGTH: u32 = 0xc0010140;
pub const X86X_AMD_MSR_OSVW_ID_STATUS: u32 = 0xc0010141;
pub const X86X_AMD_MSR_DE_CFG: u32 = 0xc0011029;

pub const DR6_BREAKPOINT_MASK: u64 = 0xf;
pub const DR6_SINGLE_STEP: u64 = 0x4000;

#[bitfield(u64, default = false)]
#[derive(PartialEq)]
pub struct RFlags {
    // FLAGS
    pub carry: bool,
    _reserved0: bool,
    pub parity: bool,
    _reserved1: bool,
    pub adjust: bool,
    _reserved2: bool,
    pub zero: bool,
    pub sign: bool,
    pub trap: bool,
    pub interrupt_enable: bool,
    pub direction: bool,
    pub overflow: bool,
    #[bits(2)]
    pub io_privilege_level: u8,
    pub nested_task: bool,
    pub mode: bool,

    // EFLAGS
    pub resume: bool,
    pub virtual_8086_mode: bool,
    pub alignment_check: bool,
    pub virtual_interrupt: bool,
    pub virtual_interrupt_pending: bool,
    pub cpuid_allowed: bool,
    _reserved3: u8,
    pub aes_key_schedule_loaded: bool,
    _reserved4: bool,

    // RFLAGS
    _reserved5: u32,
}

impl Default for RFlags {
    fn default() -> Self {
        Self(2)
    }
}

impl core::ops::BitAnd<RFlags> for RFlags {
    type Output = RFlags;

    fn bitand(self, rhs: RFlags) -> Self::Output {
        RFlags(self.0 & rhs.0)
    }
}

#[cfg(feature = "arbitrary")]
impl<'a> arbitrary::Arbitrary<'a> for RFlags {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let x: u64 = u.arbitrary()?;
        Ok(x.into())
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct IdtEntry64 {
    pub offset_low: u16,
    pub selector: u16,
    pub attributes: IdtAttributes,
    pub offset_middle: u16,
    pub offset_high: u32,
    pub reserved: u32,
}

#[bitfield(u16)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct IdtAttributes {
    #[bits(3)]
    pub ist: u8,
    #[bits(5)]
    _reserved: u8,
    #[bits(4)]
    pub gate_type: u8,
    _reserved2: bool,
    #[bits(2)]
    pub dpl: u8,
    pub present: bool,
}

#[repr(C)]
#[derive(Clone, Copy, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GdtEntry {
    pub limit_low: u16,
    pub base_low: u16,
    pub base_middle: u8,
    pub attr_low: u8,
    pub attr_high: u8,
    pub base_high: u8,
}

#[repr(C)]
#[derive(Clone, Copy, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct LargeGdtEntry {
    pub limit_low: u16,
    pub base_low: u16,
    pub base_middle: u8,
    pub attr_low: u8,
    pub attr_high: u8,
    pub base_high: u8,
    pub base_upper: u32,
    pub mbz: u32,
}

impl LargeGdtEntry {
    /// Get the large GDT entry as two smaller GDT entries, for building a GDT.
    pub fn get_gdt_entries(&self) -> [GdtEntry; 2] {
        let mut entries = [GdtEntry::new_zeroed(); 2];
        entries.as_mut_bytes().copy_from_slice(self.as_bytes());
        entries
    }
}

open_enum! {
    pub enum Exception: u8 {
        DIVIDE_ERROR = 0x0,
        DEBUG = 0x1,
        BREAKPOINT = 0x3,
        OVERFLOW = 0x4,
        BOUND_RANGE_EXCEEDED = 0x5,
        INVALID_OPCODE = 0x6,
        DEVICE_NOT_AVAILABLE = 0x7,
        DOUBLE_FAULT = 0x8,
        INVALID_TSS = 0x0A,
        SEGMENT_NOT_PRESENT = 0x0B,
        STACK_SEGMENT_FAULT = 0x0C,
        GENERAL_PROTECTION_FAULT = 0x0D,
        PAGE_FAULT = 0x0E,
        FLOATING_POINT_EXCEPTION = 0x10,
        ALIGNMENT_CHECK = 0x11,
        MACHINE_CHECK = 0x12,
        SIMD_FLOATING_POINT_EXCEPTION = 0x13,
        SEV_VMM_COMMUNICATION = 0x1D,
    }
}

#[bitfield(u32)]
pub struct PageFaultErrorCode {
    pub present: bool,
    pub write: bool,
    pub user: bool,
    pub reserved: bool,
    pub fetch: bool,
    #[bits(27)]
    _unused: u32,
}

pub const X64_LARGE_PAGE_SIZE: u64 = 0x200000;

#[bitfield(u64)]
#[derive(PartialEq, Eq, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Pte {
    pub present: bool,
    pub read_write: bool,
    pub user: bool,
    pub write_through: bool,
    pub cache_disable: bool,
    pub accessed: bool,
    pub dirty: bool,
    pub pat: bool,
    pub global: bool,
    #[bits(3)]
    pub available0: u64,
    #[bits(40)]
    pub pfn: u64,
    #[bits(11)]
    pub available1: u64,
    pub no_execute: bool,
}

impl Pte {
    pub fn address(&self) -> u64 {
        self.pfn() << 12
    }

    pub fn with_address(self, address: u64) -> Self {
        assert!(address & 0xfff == 0);
        self.with_pfn(address >> 12)
    }

    pub fn set_address(&mut self, address: u64) -> &mut Self {
        *self = self.with_address(address);
        self
    }
}

#[bitfield(u64)]
#[derive(PartialEq, Eq, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct LargePde {
    pub present: bool,
    pub read_write: bool,
    pub user: bool,
    pub write_through: bool,
    pub cache_disable: bool,
    pub accessed: bool,
    pub dirty: bool,
    pub large_page: bool,
    pub global: bool,
    #[bits(3)]
    pub available0: u64,
    pub pat: bool,
    #[bits(8)]
    _reserved0: u64,
    #[bits(31)]
    pub large_page_base: u64,
    #[bits(11)]
    pub available1: u64,
    pub no_execute: bool,
}

#[bitfield(u64)]
#[derive(PartialEq, Eq, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct X86xMcgStatusRegister {
    pub ripv: bool, // Restart IP is valid
    pub eipv: bool, // Error IP is valid
    pub mcip: bool, // Machine check is in progress
    #[bits(61)]
    pub reserved0: u64,
}
