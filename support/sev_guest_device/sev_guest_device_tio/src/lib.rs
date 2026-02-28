// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module includes the definitions of data structures according to SEV-SNP specification.

use bitfield_struct::bitfield;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// See `TIO_MSG_TDI_INFO_REQ` in Table 60, "SEV-TIO Firmware Interface Specification", Revision 0.91.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Debug)]
pub struct TioMsgTdiInfoReq {
    /// Hypervisor supplied guest id.
    pub guest_device_id: u16,
    /// Reserved
    pub _reserved0: [u8; 14],
}

static_assertions::const_assert_eq!(16, size_of::<TioMsgTdiInfoReq>());

/// See `TIO_MSG_TDI_INFO_RSP` in Table 61, "SEV-TIO Firmware Interface Specification", Revision 0.91.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Debug)]
pub struct TioMsgTdiInfoRsp {
    /// Hypervisor supplied guest id.
    pub guest_device_id: u16,
    /// TDI status.
    pub tdi_status: u16,
    /// Reserved
    pub _reserved0: [u8; 12],
    /// MEAS_DIGEST info
    pub meas_digest_info: u32,
    /// Device lock flags
    pub lock_flags: u32,
    /// SPDM algorithms
    pub spdm_algos: u64,
    /// Certs digest
    pub certs_digest: [u8; 48],
    /// MEAS digest
    pub meas_digest: [u8; 48],
    /// Interface report digest
    pub interface_report_digest: [u8; 48],
    /// Tdi report count
    pub tdi_report_count: u64,
    /// Reserved
    pub _reserved1: u64,
}

// Assert the size of the response field
static_assertions::const_assert_eq!(192, size_of::<TioMsgTdiInfoRsp>());

/// See `TIO_MSG_MMIO_VALIDATE_REQ` in Table 63, "SEV-TIO Firmware Interface Specification", Revision 0.91.
#[bitfield(u16)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TioMsgMmioValidateReqFlags {
    /// Desired value to set RMP. Validated for the range.
    pub validated: bool,

    /// 0: If subrange does not have RMP. Validated
    /// set uniformly, fail.
    /// 1: If subrange does not have RMP. Validated
    /// set uniformly, force to requested value.
    pub force_validated: bool,

    #[bits(14)]
    _reserved0: u16,
}

/// See `TIO_MSG_MMIO_VALIDATE_REQ` in Table 63, "SEV-TIO Firmware Interface Specification", Revision 0.91.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Debug)]
pub struct TioMsgMmioValidateReq {
    /// Hypervisor provided identifier used by the guest to identify the TDI in guest messages.
    pub guest_device_id: u16,
    /// Reserved.
    pub _reserved0: [u8; 14],
    /// Guest physical address of the subrange.
    pub subrange_base: u64,
    /// Number of 4 KB pages in the subrange.
    pub subrange_page_count: u32,
    /// Offset of the subrange within the MMIO range.
    pub range_offset: u32,
    /// Validated flags
    pub validated_flags: TioMsgMmioValidateReqFlags,
    /// RangeID of MMIO range.
    pub range_id: u16,
    /// Reserved.
    pub _reserved2: [u8; 12],
}

// Assert the size of the structure
static_assertions::const_assert_eq!(48, size_of::<TioMsgMmioValidateReq>());

/// See `TIO_MSG_MMIO_VALIDATE_RSP` in Table 64, "SEV-TIO Firmware Interface Specification", Revision 0.91.
#[bitfield(u16)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TioMsgMmioValidateResFlags {
    /// Indicates that the Validated bit has changed due to this operation.
    pub changed: bool,

    #[bits(15)]
    _reserved0: u16,
}

/// See `TIO_MSG_MMIO_VALIDATE_RSP` in Table 64, "SEV-TIO Firmware Interface Specification", Revision 0.91.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Debug)]
pub struct TioMsgMmioValidateRsp {
    /// Hypervisor provided PCIe Routing ID used by the guest to identify the TDI.
    pub guest_device_id: u16,
    /// Status of the operation.
    pub status: u16,
    /// Reserved.
    pub _reserved0: [u8; 12],
    /// Guest physical address of the subrange.
    pub subrange_base: u64,
    /// Number of 4 KB pages in the subrange.
    pub subrange_page_count: u32,
    /// Offset of the subrange within the MMIO range.
    pub range_offset: u32,
    /// Validated flags
    pub flag_bits: TioMsgMmioValidateResFlags,
    /// Range of the MMIO.
    pub range_id: u16,
    /// Reserved.
    pub _reserved2: [u8; 12],
}

// Assert the size of the structure
static_assertions::const_assert_eq!(48, size_of::<TioMsgMmioValidateRsp>());

/// See `TIO_MSG_MMIO_CONFIG_REQ` in Table 65, "SEV-TIO Firmware Interface Specification", Revision 0.91.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Debug)]
pub struct TioMsgMmioConfigReq {
    /// Hypervisor provided identifier used by the guest to identify the TDI in guest messages.
    pub guest_device_id: u16,
    /// Reserved.
    pub _reserved0: [u8; 2],
    /// Flags for the range.
    pub flags: u16,
    /// Range ID of the MMIO range.
    pub range_id: u16,
    /// WRITE flag.
    pub write: u32,
    /// Reserved.
    pub _reserved2: [u8; 4],
}

impl TioMsgMmioConfigReq {
    /// 0: Can be mapped only into guest private
    /// memory.
    /// 1: Can be mapped into either guest
    /// private memory or shared memory.
    /// Ignored if WRITE is 0.
    pub fn is_non_tee_mem(&self) -> bool {
        (self.flags & (1 << 2)) != 0
    }
}

static_assertions::const_assert_eq!(16, size_of::<TioMsgMmioConfigReq>());

/// See `TIO_MSG_MMIO_CONFIG_RSP` in Table 66, "SEV-TIO Firmware Interface Specification", Revision 0.91.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Debug)]
pub struct TioMsgMmioConfigRsp {
    /// Hypervisor provided identifier used by the guest to identify the TDI in guest messages.
    pub guest_device_id: u16,
    /// Status of the operation.
    pub status: u16,
    /// Flags for the range.
    pub flags: u16,
    /// Range ID of the MMIO range.
    pub range_id: u16,
    /// WRITE flag.
    pub write: u32,
    /// Reserved.
    pub _reserved1: [u8; 4],
}

impl TioMsgMmioConfigRsp {
    /// Indicates if certain TDISP flags can be updated.
    pub fn is_mem_attr_updateable(&self) -> bool {
        (self.flags & (1 << 3)) != 0
    }

    /// Indicates if the range can be mapped into either guest private memory or shared memory.
    pub fn is_non_tee_mem(&self) -> bool {
        (self.flags & (1 << 2)) != 0
    }

    /// Indicates if this range maps MSI-X PBA.
    pub fn msix_pba(&self) -> bool {
        (self.flags & (1 << 1)) != 0
    }

    /// Indicates if the range maps MSI-X table.
    pub fn msix_table(&self) -> bool {
        (self.flags & 1) != 0
    }
}

static_assertions::const_assert_eq!(16, size_of::<TioMsgMmioConfigRsp>());

/// See `Layout of the SDTE Structure` in Table 68, "SEV-TIO Firmware Interface Specification", Revision 0.91.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SdtePart1 {
    // [0]
    pub v: bool,

    #[bits(60)]
    _reserved0: u64,

    pub ir: bool,

    pub iw: bool,

    _reserved1: bool,
}

/// See `Layout of the SDTE Structure` in Table 68, "SEV-TIO Firmware Interface Specification", Revision 0.91.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SdtePart2 {
    #[bits(49)]
    pub _reserved0: u64,

    #[bits(2)]
    pub vmpl: u64,

    #[bits(13)]
    _reserved1: u64,
}

/// See `Layout of the SDTE Structure` in Table 68, "SEV-TIO Firmware Interface Specification", Revision 0.91.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SdtePart3 {
    pub vtom_en: bool,

    #[bits(31)]
    pub virtual_tom: u32,

    #[bits(32)]
    _reserved1: u64,
}

/// See `Layout of the SDTE Structure` in Table 68, "SEV-TIO Firmware Interface Specification", Revision 0.91.
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Debug)]
#[repr(C)]
pub struct Sdte {
    /// Part 1 of the guest writable portion of the SDTE structure. These are split out to preserve specific alignment requirements from the spec.
    pub part1: SdtePart1,
    /// Reserved. Set to 0.
    pub _reserved0: u64,
    /// Reserved. Set to 0.
    pub _reserved1: u64,
    /// Part 2 of the guest writable portion of the SDTE structure. These are split out to preserve specific alignment requirements from the spec.
    pub part2: SdtePart2,
    /// Reserved. Set to 0.
    pub _reserved2: u64,

    /// Part 3 of the guest writable portion of the SDTE structure. These are split out to preserve specific alignment requirements from the spec.
    pub part3: SdtePart3,
    /// Reserved. Set to 0.
    pub _reserved3: u64,
    /// Reserved. Set to 0.
    pub _reserved4: u64,
}

static_assertions::const_assert_eq!(size_of::<Sdte>(), 64);

/// See `TIO_MSG_SDTE_WRITE_REQ` in Table 67, "SEV-TIO Firmware Interface Specification", Revision 0.91.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Debug)]
pub struct TioMsgSdteWriteReq {
    /// Hypervisor provided identifier used by the guest to identify the TDI in guest messages.
    pub guest_device_id: u16,

    /// Reserved. Set to 0.
    pub _reserved0: [u8; 14],

    /// sDTE to use to configure the guest controlled fields.
    pub sdte: Sdte,
}

/// See `TIO_MSG_SDTE_WRITE_RSP` in Table 69, "SEV-TIO Firmware Interface Specification", Revision 0.91.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Debug)]
pub struct TioMsgSdteWriteRsp {
    /// Hypervisor provided PCIe Routing ID used by the guest to identify the TDI.
    pub guest_device_id: u16,
    /// Status of the operation.
    pub status: u16,
    /// Reserved.
    pub _reserved0: [u8; 12],
}

static_assertions::const_assert_eq!(size_of::<TioMsgSdteWriteRsp>(), 16);
