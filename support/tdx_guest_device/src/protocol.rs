// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module includes the definitions of data structures according to TDX specification.

use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Ioctl type defined by Linux.
pub const TDX_CMD_GET_REPORT0_IOC_TYPE: u8 = b'T';

/// Size of the [`TdReport`].
pub const TDX_REPORT_SIZE: usize = 0x400;

/// Size of `report_data` member in [`ReportMac`].
pub const TDX_REPORT_DATA_SIZE: usize = 64;

/// Ioctl struct defined by Linux.
#[repr(C)]
pub struct TdxReportReq {
    /// Report data to be included in the report.
    pub report_data: [u8; TDX_REPORT_DATA_SIZE],
    /// The output report.
    pub td_report: TdReport,
}

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

/// See `TDINFO_BASE` in Table 3.34, "Intel TDX Module v1.5 ABI specification", March 2024.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TdInfoBase {
    /// TD's attributes
    pub attributes: [u8; 8],
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
