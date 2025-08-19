// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Intel TDX specific definitions.

use crate::vmx;
use bitfield_struct::bitfield;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

pub const TDX_SHARED_GPA_BOUNDARY_BITS: u8 = 47;
pub const TDX_SHARED_GPA_BOUNDARY_ADDRESS_BIT: u64 = 1 << TDX_SHARED_GPA_BOUNDARY_BITS;
pub const RESET_VECTOR_PAGE: u64 = 0xfffff000;

/// Size of the [`TdReport`].
pub const TDX_REPORT_SIZE: usize = 0x400;

/// Size of `report_data` member in [`ReportMac`].
pub const TDX_REPORT_DATA_SIZE: usize = 64;

open_enum! {
    /// TDCALL instruction leafs that are passed into the tdcall instruction
    /// in eax.
    pub enum TdCallLeaf: u64 {
        VP_VMCALL = 0,
        VP_INFO = 1,
        MR_RTMR_EXTEND = 2,
        VP_VEINFO_GET = 3,
        MR_REPORT = 4,
        VP_CPUIDVE_SET = 5,
        MEM_PAGE_ACCEPT = 6,
        VM_RD = 7,
        VM_WR = 8,
        VP_RD = 9,
        VP_WR = 10,
        MEM_PAGE_ATTR_RD = 23,
        MEM_PAGE_ATTR_WR = 24,
        VP_ENTER = 25,
        VP_INVGLA = 27,
    }
}

/// Level used in various TDG.MEM.PAGE calls for GPA_MAPPING types.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TdgMemPageLevel {
    Size4k = 0,
    Size2Mb = 1,
    Size1Gb = 2,
}

impl TdgMemPageLevel {
    const fn from_bits(value: u64) -> Self {
        match value {
            0 => Self::Size4k,
            1 => Self::Size2Mb,
            2 => Self::Size1Gb,
            _ => panic!("invalid TdgMemPageLevel value"),
        }
    }

    const fn into_bits(self) -> u64 {
        self as u64
    }
}

/// Attributes for a single VM.
#[bitfield(u16)]
#[derive(PartialEq, Eq)]
pub struct GpaVmAttributes {
    pub read: bool,
    pub write: bool,
    pub kernel_execute: bool,
    pub user_execute: bool,
    #[bits(3)]
    reserved: u8,
    suppress_ve: bool,
    #[bits(7)]
    reserved2: u8,
    pub valid: bool,
}

// Required impls for using within bitfield macros in other structs.
impl GpaVmAttributes {
    pub const FULL_ACCESS: Self = Self::new()
        .with_read(true)
        .with_write(true)
        .with_kernel_execute(true)
        .with_user_execute(true)
        .with_valid(true);
}

/// Attributes mask used to set which bits are updated in TDG.MEM.PAGE.ATTR.WR.
#[bitfield(u16)]
pub struct GpaVmAttributesMask {
    pub read: bool,
    pub write: bool,
    pub kernel_execute: bool,
    pub user_execute: bool,
    #[bits(3)]
    reserved: u8,
    pub suppress_ve: bool,
    #[bits(7)]
    reserved2: u8,
    /// invalidate ept for this vm
    pub inv_ept: bool,
}

impl GpaVmAttributesMask {
    pub const ALL_CHANGED: Self = Self::new()
        .with_read(true)
        .with_write(true)
        .with_kernel_execute(true)
        .with_user_execute(true);
}

/// Corresponds to GPA_ATTR, which is used as input to TDG.MEM.PAGE.ATTR.WR and
/// returned from TDG.MEM.PAGE.ATTR.RD.
#[bitfield(u64)]
#[derive(PartialEq, Eq)]
pub struct TdgMemPageGpaAttr {
    /// represents L1 vm aka VTL2
    #[bits(16)]
    pub l1: GpaVmAttributes,
    /// Represetns L2 vm #1 which we use as VTL0
    #[bits(16)]
    pub l2_vm1: GpaVmAttributes,
    /// Represents L2 vm #2 which we use as VTL1
    #[bits(16)]
    pub l2_vm2: GpaVmAttributes,
    #[bits(16)]
    pub l2_vm3: GpaVmAttributes,
}

#[bitfield(u64)]
pub struct TdgMemPageAcceptRcx {
    #[bits(3)]
    pub level: TdgMemPageLevel,
    #[bits(9)]
    pub reserved: u64,
    /// The page number for this accept call.
    #[bits(40)]
    pub gpa_page_number: u64,
    #[bits(12)]
    pub reserved2: u64,
}

#[bitfield(u64)]
pub struct TdgMemPageAttrGpaMappingReadRcxResult {
    #[bits(3)]
    pub level: TdgMemPageLevel,
    #[bits(9)]
    pub reserved: u64,
    /// The page number for this accept call.
    #[bits(40)]
    pub gpa_page_number: u64,
    #[bits(10)]
    pub reserved2: u64,
    /// If this page's attributes are pending, meaning it will be applied when
    /// the page is accepted.
    #[bits(1)]
    pub pending: u8,
    #[bits(1)]
    pub reserved3: u64,
}

/// RCX input to TDG.MEM.PAGE.ATTR.WR.
#[bitfield(u64)]
#[derive(PartialEq, Eq)]
pub struct TdgMemPageAttrWriteRcx {
    #[bits(3)]
    pub level: TdgMemPageLevel,
    #[bits(9)]
    pub reserved: u64,
    /// The page number for this write call.
    #[bits(40)]
    pub gpa_page_number: u64,
    #[bits(12)]
    pub reserved2: u64,
}

/// R8 input to TDG.MEM.PAGE.ATTR.WR.
#[bitfield(u64)]
pub struct TdgMemPageAttrWriteR8 {
    #[bits(16)]
    pub reserved: u64,
    /// Corresponds to ATTR_MASK1
    #[bits(16)]
    pub l2_vm1: GpaVmAttributesMask,
    /// Corresponds to ATTR_MASK2
    #[bits(16)]
    pub l2_vm2: GpaVmAttributesMask,
    /// Corresponds to ATTR_MASK3
    #[bits(16)]
    pub l2_vm3: GpaVmAttributesMask,
}

/// The value specified in `r11` when making a TD vmcall, specified by `r10 =
/// 0`.
#[repr(u64)]
pub enum TdVmCallSubFunction {
    IoInstr = 0x1e,
    RdMsr = 0x1f,
    WrMsr = 0x20,
    MapGpa = 0x10001,
}

open_enum! {
    /// Result code for `tdcall` to the TDX module, returned in RAX.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum TdCallResultCode: u32 {
        SUCCESS = 0x00000000,
        NON_RECOVERABLE_VCPU = 0x40000001,
        NON_RECOVERABLE_TD = 0x60000002,
        INTERRUPTED_RESUMABLE = 0x80000003,
        INTERRUPTED_RESTARTABLE = 0x80000004,
        NON_RECOVERABLE_TD_NON_ACCESSIBLE = 0x60000005,
        INVALID_RESUMPTION = 0xC0000006,
        NON_RECOVERABLE_TD_WRONG_APIC_MODE = 0xE0000007,
        CROSS_TD_FAULT = 0x80000008,
        CROSS_TD_TRAP = 0x90000009,
        NON_RECOVERABLE_TD_CORRUPTED_MD = 0x6000000A,
        OPERAND_INVALID = 0xC0000100,
        OPERAND_ADDR_RANGE_ERROR = 0xC0000101,
        OPERAND_BUSY = 0x80000200,
        PREVIOUS_TLB_EPOCH_BUSY = 0x80000201,
        SYS_BUSY = 0x80000202,
        RND_NO_ENTROPY = 0x80000203,
        OPERAND_BUSY_HOST_PRIORITY = 0x80000204,
        HOST_PRIORITY_BUSY_TIMEOUT = 0x90000205,
        PAGE_METADATA_INCORRECT = 0xC0000300,
        PAGE_ALREADY_FREE = 0x00000301,
        PAGE_NOT_OWNED_BY_TD = 0xC0000302,
        PAGE_NOT_FREE = 0xC0000303,
        TD_ASSOCIATED_PAGES_EXIST = 0xC0000400,
        SYS_INIT_NOT_PENDING = 0xC0000500,
        SYS_LP_INIT_NOT_DONE = 0xC0000502,
        SYS_LP_INIT_DONE = 0xC0000503,
        SYS_NOT_READY = 0xC0000505,
        SYS_SHUTDOWN = 0xC0000506,
        SYS_KEY_CONFIG_NOT_PENDING = 0xC0000507,
        SYS_STATE_INCORRECT = 0xC0000508,
        SYS_INVALID_HANDOFF = 0xC0000509,
        SYS_INCOMPATIBLE_SIGSTRUCT = 0xC000050A,
        SYS_LP_INIT_NOT_PENDING = 0xC000050B,
        SYS_CONFIG_NOT_PENDING = 0xC000050C,
        INCOMPATIBLE_SEAM_CAPABILITIES = 0xC000050D,
        TD_FATAL = 0xE0000604,
        TD_NON_DEBUG = 0xC0000605,
        TDCS_NOT_ALLOCATED = 0xC0000606,
        LIFECYCLE_STATE_INCORRECT = 0xC0000607,
        OP_STATE_INCORRECT = 0xC0000608,
        NO_VCPUS = 0xC0000609,
        TDCX_NUM_INCORRECT = 0xC0000610,
        VCPU_STATE_INCORRECT = 0xC0000700,
        VCPU_ASSOCIATED = 0x80000701,
        VCPU_NOT_ASSOCIATED = 0x80000702,
        NO_VALID_VE_INFO = 0xC0000704,
        MAX_VCPUS_EXCEEDED = 0xC0000705,
        TSC_ROLLBACK = 0xC0000706,
        TD_VMCS_FIELD_NOT_INITIALIZED = 0xC0000730,
        MCS_FIELD_ERROR = 0xC0000731,
        KEY_GENERATION_FAILED = 0x80000800,
        TD_KEYS_NOT_CONFIGURED = 0x80000810,
        KEY_STATE_INCORRECT = 0xC0000811,
        KEY_CONFIGURED = 0x00000815,
        WBCACHE_NOT_COMPLETE = 0x80000817,
        HKID_NOT_FREE = 0xC0000820,
        NO_HKID_READY_TO_WBCACHE = 0x00000821,
        WBCACHE_RESUME_ERROR = 0xC0000823,
        FLUSHVP_NOT_DONE = 0x80000824,
        NUM_ACTIVATED_HKIDS_NOT_SUPPORTED = 0xC0000825,
        INCORRECT_CPUID_VALUE = 0xC0000900,
        LIMIT_CPUID_MAXVAL_SET = 0xC0000901,
        INCONSISTENT_CPUID_FIELD = 0xC0000902,
        CPUID_MAX_SUBLEAVES_UNRECOGNIZED = 0xC0000903,
        CPUID_LEAF_1F_FORMAT_UNRECOGNIZED = 0xC0000904,
        INVALID_WBINVD_SCOPE = 0xC0000905,
        INVALID_PKG_ID = 0xC0000906,
        ENABLE_MONITOR_FSM_NOT_SET = 0xC0000907,
        CPUID_LEAF_NOT_SUPPORTED = 0xC0000908,
        SMRR_NOT_LOCKED = 0xC0000910,
        INVALID_SMRR_CONFIGURATION = 0xC0000911,
        SMRR_OVERLAPS_CMR = 0xC0000912,
        SMRR_LOCK_NOT_SUPPORTED = 0xC0000913,
        SMRR_NOT_SUPPORTED = 0xC0000914,
        INCONSISTENT_MSR = 0xC0000920,
        INCORRECT_MSR_VALUE = 0xC0000921,
        SEAMREPORT_NOT_AVAILABLE = 0xC0000930,
        SEAMDB_GETREF_NOT_AVAILABLE = 0xC0000931,
        SEAMDB_REPORT_NOT_AVAILABLE = 0xC0000932,
        SEAMVERIFYREPORT_NOT_AVAILABLE = 0xC0000933,
        INVALID_TDMR = 0xC0000A00,
        NON_ORDERED_TDMR = 0xC0000A01,
        TDMR_OUTSIDE_CMRS = 0xC0000A02,
        TDMR_ALREADY_INITIALIZED = 0x00000A03,
        INVALID_PAMT = 0xC0000A10,
        PAMT_OUTSIDE_CMRS = 0xC0000A11,
        PAMT_OVERLAP = 0xC0000A12,
        INVALID_RESERVED_IN_TDMR = 0xC0000A20,
        NON_ORDERED_RESERVED_IN_TDMR = 0xC0000A21,
        CMR_LIST_INVALID = 0xC0000A22,
        EPT_WALK_FAILED = 0xC0000B00,
        EPT_ENTRY_FREE = 0xC0000B01,
        EPT_ENTRY_NOT_FREE = 0xC0000B02,
        EPT_ENTRY_NOT_PRESENT = 0xC0000B03,
        EPT_ENTRY_NOT_LEAF = 0xC0000B04,
        EPT_ENTRY_LEAF = 0xC0000B05,
        GPA_RANGE_NOT_BLOCKED = 0xC0000B06,
        GPA_RANGE_ALREADY_BLOCKED = 0x00000B07,
        TLB_TRACKING_NOT_DONE = 0xC0000B08,
        EPT_INVALID_PROMOTE_CONDITIONS = 0xC0000B09,
        PAGE_ALREADY_ACCEPTED = 0x00000B0A,
        PAGE_SIZE_MISMATCH = 0xC0000B0B,
        GPA_RANGE_BLOCKED = 0xC0000B0C,
        EPT_ENTRY_STATE_INCORRECT = 0xC0000B0D,
        EPT_PAGE_NOT_FREE = 0xC0000B0E,
        L2_SEPT_WALK_FAILED = 0xC0000B0F,
        L2_SEPT_ENTRY_NOT_FREE = 0xC0000B10,
        PAGE_ATTR_INVALID = 0xC0000B11,
        L2_SEPT_PAGE_NOT_PROVIDED = 0xC0000B12,
        METADATA_FIELD_ID_INCORRECT = 0xC0000C00,
        METADATA_FIELD_NOT_WRITABLE = 0xC0000C01,
        METADATA_FIELD_NOT_READABLE = 0xC0000C02,
        METADATA_FIELD_VALUE_NOT_VALID = 0xC0000C03,
        METADATA_LIST_OVERFLOW = 0xC0000C04,
        INVALID_METADATA_LIST_HEADER = 0xC0000C05,
        REQUIRED_METADATA_FIELD_MISSING = 0xC0000C06,
        METADATA_ELEMENT_SIZE_INCORRECT = 0xC0000C07,
        METADATA_LAST_ELEMENT_INCORRECT = 0xC0000C08,
        METADATA_FIELD_CURRENTLY_NOT_WRITABLE = 0xC0000C09,
        METADATA_WR_MASK_NOT_VALID = 0xC0000C0A,
        METADATA_FIRST_FIELD_ID_IN_CONTEXT = 0x00000C0B,
        METADATA_FIELD_SKIP = 0x00000C0C,
        SERVTD_ALREADY_BOUND_FOR_TYPE = 0xC0000D00,
        SERVTD_TYPE_MISMATCH = 0xC0000D01,
        SERVTD_ATTR_MISMATCH = 0xC0000D02,
        SERVTD_INFO_HASH_MISMATCH = 0xC0000D03,
        SERVTD_UUID_MISMATCH = 0xC0000D04,
        SERVTD_NOT_BOUND = 0xC0000D05,
        SERVTD_BOUND = 0xC0000D06,
        TARGET_UUID_MISMATCH = 0xC0000D07,
        TARGET_UUID_UPDATED = 0xC0000D08,
        INVALID_MBMD = 0xC0000E00,
        INCORRECT_MBMD_MAC = 0xC0000E01,
        NOT_WRITE_BLOCKED = 0xC0000E02,
        ALREADY_WRITE_BLOCKED = 0x00000E03,
        NOT_EXPORTED = 0xC0000E04,
        MIGRATION_STREAM_STATE_INCORRECT = 0xC0000E05,
        MAX_MIGS_NUM_EXCEEDED = 0xC0000E06,
        EXPORTED_DIRTY_PAGES_REMAIN = 0xC0000E07,
        MIGRATION_DECRYPTION_KEY_NOT_SET = 0xC0000E08,
        TD_NOT_MIGRATABLE = 0xC0000E09,
        PREVIOUS_EXPORT_CLEANUP_INCOMPLETE = 0xC0000E0A,
        NUM_MIGS_HIGHER_THAN_CREATED = 0xC0000E0B,
        IMPORT_MISMATCH = 0xC0000E0C,
        MIGRATION_EPOCH_OVERFLOW = 0xC0000E0D,
        MAX_EXPORTS_EXCEEDED = 0xC0000E0E,
        INVALID_PAGE_MAC = 0xC0000E0F,
        MIGRATED_IN_CURRENT_EPOCH = 0xC0000E10,
        DISALLOWED_IMPORT_OVER_REMOVED = 0xC0000E11,
        SOME_VCPUS_NOT_MIGRATED = 0xC0000E12,
        ALL_VCPUS_IMPORTED = 0xC0000E13,
        MIN_MIGS_NOT_CREATED = 0xC0000E14,
        VCPU_ALREADY_EXPORTED = 0xC0000E15,
        INVALID_MIGRATION_DECRYPTION_KEY = 0xC0000E16,
        INVALID_CPUSVN = 0xC0001000,
        INVALID_REPORTMACSTRUCT = 0xC0001001,
        L2_EXIT_HOST_ROUTED_ASYNC = 0x00001100,
        L2_EXIT_HOST_ROUTED_TDVMCALL = 0x00001101,
        L2_EXIT_PENDING_INTERRUPT = 0x00001102,
        PENDING_INTERRUPT = 0x00001120,
        TD_EXIT_BEFORE_L2_ENTRY = 0x00001140,
        TD_EXIT_ON_L2_VM_EXIT = 0x00001141,
        TD_EXIT_ON_L2_TO_L1 = 0x00001142,
        GLA_NOT_CANONICAL = 0xC0001160,
    }
}

impl TdCallResultCode {
    const fn from_bits(value: u64) -> Self {
        Self(value as u32)
    }

    const fn into_bits(self) -> u64 {
        self.0 as u64
    }
}

/// The result returned by a tdcall instruction in rax.
#[bitfield(u64)]
pub struct TdCallResult {
    pub details: u32,
    #[bits(32)]
    pub code: TdCallResultCode,
}

open_enum! {
    /// The result returned by a tdg.vm.call in r10.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum TdVmCallR10Result: u64 {
        SUCCESS = 0,
        RETRY = 1,
        OPERAND_INVALID = 0x80000000_00000000,
        GPA_INUSE = 0x80000000_00000001,
        ALIGN_ERROR = 0x80000000_00000002,
    }
}

/// Field size for [`TdxExtendedFieldCode`].
#[repr(u64)]
#[derive(Debug)]
pub enum FieldSize {
    Invalid = 0,
    Size16Bit = 1,
    Size32Bit = 2,
    Size64Bit = 3,
}

impl FieldSize {
    const fn from_bits(value: u64) -> Self {
        match value {
            0 => FieldSize::Invalid,
            1 => FieldSize::Size16Bit,
            2 => FieldSize::Size32Bit,
            3 => FieldSize::Size64Bit,
            _ => panic!("Invalid field size"),
        }
    }

    const fn into_bits(self) -> u64 {
        self as u64
    }
}

open_enum! {
    pub enum TdVpsClassCode: u8 {
        TD_VMCS = 0,
        VAPIC = 1,
        VE_INFO = 2,
        GUEST_GPR_STATE = 16,
        GUEST_STATE = 17,
        GUEST_EXT_STATE = 18,
        GUEST_MSR_STATE = 19,
        MANAGEMENT = 32,
        CPUID_CONTROL = 33,
        EPT_VIOLATION_LOG = 34,
        VMCS_1 = 36,
        MSR_BITMAPS_1 = 37,
        MSR_BITMAPS_SHADOW_1 = 38,
        VMCS_2 = 44,
        MSR_BITMAPS_2 = 45,
        MSR_BITMAPS_SHADOW_2 = 46,
        VMCS_3 = 52,
    }
}

open_enum! {
    pub enum TdxContextCode: u8 {
        PLATFORM = 0,
        TD = 1,
        TD_VCPU = 2,
    }
}

impl TdxContextCode {
    const fn from_bits(value: u64) -> Self {
        Self(value as u8)
    }
    const fn into_bits(self) -> u64 {
        self.0 as u64
    }
}

pub const TDX_FIELD_CODE_L2_CTLS_VM1: TdxExtendedFieldCode =
    TdxExtendedFieldCode(0xA020000300000051);
pub const TDX_FIELD_CODE_L2_CTLS_VM2: TdxExtendedFieldCode =
    TdxExtendedFieldCode(0xA020000300000052);

/// Extended field code for TDG.VP.WR and TDG.VP.RD
#[bitfield(u64)]
pub struct TdxExtendedFieldCode {
    #[bits(24)]
    pub field_code: u32,
    #[bits(8)]
    _reserved0: u64,
    #[bits(2)]
    pub field_size: FieldSize,
    #[bits(4)]
    pub last_element: u8,
    #[bits(9)]
    pub last_field: u16,
    #[bits(3)]
    _reserved1: u64,
    pub increment_size: bool,
    pub write_mask_valid: bool,
    #[bits(3)]
    pub context_code: TdxContextCode,
    #[bits(1)]
    _reserved2: u64,
    #[bits(6)]
    pub class_code: u8,
    #[bits(1)]
    _reserved3: u64,
    pub non_arch: bool,
}

/// Instruction info returned in r11 for a TDG.VP.ENTER call.
#[bitfield(u64)]
pub struct TdxInstructionInfo {
    pub info: u32,
    pub length: u32,
}

#[bitfield(u64)]
pub struct TdxL2Ctls {
    pub enable_shared_ept: bool,
    pub enable_tdvmcall: bool,
    #[bits(62)]
    pub reserved: u64,
}

#[bitfield(u64)]
pub struct TdxVpEnterRaxResult {
    /// The VMX exit code for VP.ENTER, if valid.
    #[bits(32)]
    pub vmx_exit: vmx::VmxExit,
    /// The TDX specific exit code.
    #[bits(32)]
    pub tdx_exit: TdCallResultCode,
}

#[bitfield(u64)]
pub struct TdxExtendedExitQualification {
    #[bits(4)]
    pub ty: TdxExtendedExitQualificationType,
    #[bits(60)]
    _reserved: u64,
}

open_enum! {
    pub enum TdxExtendedExitQualificationType: u8 {
        NONE = 0,
        PENDING_EPT_VIOLATION = 6,
    }
}

impl TdxExtendedExitQualificationType {
    const fn from_bits(value: u64) -> Self {
        Self(value as u8)
    }

    const fn into_bits(self) -> u64 {
        self.0 as u64
    }
}

/// The GPR list used for TDG.VP.ENTER. Specified in the TDX specification as
/// L2_ENTER_GUEST_STATE.
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TdxL2EnterGuestState {
    /// GPs in the usual order.
    pub gps: [u64; 16],
    pub rflags: u64,
    pub rip: u64,
    pub ssp: u64,
    pub rvi: u8, // GUEST_INTERRUPT_STATUS lower bits
    pub svi: u8, // GUEST_INTERRUPT_STATUS upper bits
    pub reserved: [u8; 6],
}

pub enum TdxGp {}
impl TdxGp {
    pub const RAX: usize = 0;
    pub const RCX: usize = 1;
    pub const RDX: usize = 2;
    pub const RBX: usize = 3;
    pub const RSP: usize = 4;
    pub const RBP: usize = 5;
    pub const RSI: usize = 6;
    pub const RDI: usize = 7;
    pub const R8: usize = 8;
    pub const R9: usize = 9;
    pub const R10: usize = 10;
    pub const R11: usize = 11;
    pub const R12: usize = 12;
    pub const R13: usize = 13;
    pub const R14: usize = 14;
    pub const R15: usize = 15;
}

#[bitfield(u64)]
pub struct TdxGlaListInfo {
    #[bits(9)]
    pub first_entry: u64,
    #[bits(3)]
    _reserved_z0: u64,
    #[bits(40)]
    pub list_gpa: u64,
    #[bits(10)]
    pub num_entries: u64,
    #[bits(2)]
    _reserved_z1: u64,
}

#[bitfield(u64)]
pub struct TdGlaVmAndFlags {
    pub list: bool,
    #[bits(51)]
    _reserved_z0: u64,
    #[bits(2)]
    pub vm_index: u64,
    #[bits(10)]
    _reserved_z1: u64,
}

#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TdxVmFlags {
    #[bits(2)]
    pub invd_translations: u8,

    #[bits(50)]
    _reserved: u64,

    /// Starts at 1, not 0.
    #[bits(2)]
    pub vm_index: u8,

    #[bits(10)]
    _reserved_2: u64,
}

pub const TDX_VP_ENTER_INVD_INVEPT: u8 = 1;
pub const TDX_VP_ENTER_INVD_INVVPID: u8 = 2;
pub const TDX_VP_ENTER_INVD_INVVPID_NON_GLOBAL: u8 = 3;

/// Report structure.
/// See `TDREPORT_STRUCT` in Table 3.29, "Intel TDX Module v1.5 ABI specification", March 2024.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TdReport {
    /// An instance of [`ReportMac`]
    pub report_mac_struct: ReportMac,
    /// An instance of [`TeeTcbInfo`].
    pub tee_tcb_info: TeeTcbInfo,
    /// Reserved
    pub _reserved: [u8; 17],
    /// An instance of [`TdInfo`].
    pub td_info: TdInfo,
}

static_assertions::const_assert_eq!(TDX_REPORT_SIZE, size_of::<TdReport>());

/// See `REPORTMACSTRUCT` in Table 3.31, "Intel TDX Module v1.5 ABI specification", March 2024.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReportMac {
    /// Type header structure
    pub report_type: ReportType,
    /// Must be zero
    pub _reserved0: [u8; 12],
    /// CPU SVN
    pub cpu_svn: [u8; 16],
    /// SHA384 of [`TeeTcbInfo`]
    pub tee_tcb_info_hash: [u8; 48],
    /// SHA384 of [`TdInfo`] for TDX
    pub tee_info_hash: [u8; 48],
    /// A set of data used for communication between the caller and the target
    pub report_data: [u8; TDX_REPORT_DATA_SIZE],
    /// Must be zero
    pub _reserved1: [u8; 32],
    /// The MAC over above data.
    pub mac: [u8; 32],
}

/// See `REPORTTYPE` in Table 3.32, "Intel TDX Module v1.5 ABI specification", March 2024.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReportType {
    /// TEE type
    /// 0x00: SGX
    /// 0x81: TDX
    pub tee_type: u8,
    /// TEE type-specific subtype
    /// 0: Standard TDX report
    pub sub_type: u8,
    /// TEE type-specific version
    /// For TDX
    ///    0: `TDINFO_STRUCT.SERVTD_HASH` is not used (all 0's)
    ///    1: `TDINFO_STRUCT.SERVTD_HASH` is used
    pub version: u8,
    /// Must be zero
    pub _reserved: u8,
}

/// See `TEE_TCB_INFO` in Table 3.29, "Intel TDX Module v1.5 ABI specification", March 2024.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TeeTcbInfo {
    /// Indicates which fields are valid.
    /// Set to 0x301ff.
    pub valid: [u8; 8],
    /// [`TeeTcbSvn`] of the TDX module that created the TD on the current
    /// platform.
    pub tee_tcb_svn: TeeTcbSvn,
    /// The measurement of the TDX module that created the TD on the
    /// current platform.
    pub mr_seam: [u8; 48],
    /// Set to all 0's.
    pub mr_signer_seam: [u8; 48],
    /// Set to all 0's.
    pub attributes: [u8; 8],
    /// [`TeeTcbSvn`] of the current TDX module on the current platform.
    pub tee_tcb_svn2: TeeTcbSvn,
    /// Reserved
    pub reserved: [u8; 95],
}

/// See `TEE_TCB_SVN` in Section 3.9.4, "Intel TDX Module v1.5 ABI specification", March 2024.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TeeTcbSvn {
    /// TDX module minor SVN
    pub tdx_module_svn_minor: u8,
    /// TDX module major SVN
    pub tdx_module_svn_major: u8,
    /// Microcode SE_SVN at the time the TDX module was loaded
    pub seam_last_patch_svn: u8,
    /// Reserved
    pub _reserved: [u8; 13],
}

/// See `TDINFO_STRUCT` in Table 3.33, "Intel TDX Module v1.5 ABI specification", March 2024.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TdInfo {
    /// An instance of [`TdInfoBase`]
    pub td_info_base: TdInfoBase,
    /// Must be zero when `version` in [`ReportType`] is 0 or 1.
    pub td_info_extension: [u8; 64],
}

/// Run-time extendable measurement register.
pub type Rtmr = [u8; 48];

/// See `ATTRIBUTES` in Table 3.9, "Intel TDX Module v1.5 ABI specification", March 2024.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TdAttributes {
    #[bits(1)]
    pub debug: bool,
    #[bits(3)]
    _reserved1: u8,
    #[bits(1)]
    pub hgs_plus_prof: bool,
    #[bits(1)]
    pub perf_prof: bool,
    #[bits(1)]
    pub pmt_prof: bool,
    #[bits(9)]
    _reserved2: u16,
    #[bits(7)]
    _reserved_p: u8,
    #[bits(4)]
    _reserved_n: u8,
    #[bits(1)]
    pub lass: bool,
    #[bits(1)]
    pub sept_ve_disable: bool,
    #[bits(1)]
    pub migratable: bool,
    #[bits(1)]
    pub pks: bool,
    #[bits(1)]
    pub kl: bool,
    #[bits(24)]
    _reserved3: u32,
    #[bits(6)]
    _reserved4: u32,
    #[bits(1)]
    pub tpa: bool,
    #[bits(1)]
    pub perfmon: bool,
}

/// See `TDINFO_BASE` in Table 3.34, "Intel TDX Module v1.5 ABI specification", March 2024.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TdInfoBase {
    /// TD's attributes
    pub attributes: TdAttributes,
    /// TD's XFAM
    pub xfam: [u8; 8],
    /// Measurement of the initial contents of the TDX in SHA384
    pub mr_td: [u8; 48],
    /// Software-defined ID for non-owner-defined configuration of the guest TD
    /// in SHA384
    pub mr_config_id: [u8; 48],
    /// Software-defined ID for the guest TD's owner in SHA384
    pub mr_owner: [u8; 48],
    /// Software-defined ID for owner-defined configuration of the guest TD
    /// in SHA384
    pub mr_owner_config: [u8; 48],
    /// Array of 4 [`Rtmr`]
    pub rtmr: [Rtmr; 4],
    /// SHA384 of the `TDINFO_STRUCTs` of bound service TDs if there is any.
    pub servd_hash: [u8; 48],
}
