// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use bitfield_struct::bitfield;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// PCI Express Base Specification Revision 6.3 Section 11.3.11 DEVICE_INTERFACE_REPORT
#[bitfield(u16)]
#[derive(KnownLayout, FromBytes, Immutable, IntoBytes)]
pub struct TdispTdiReportInterfaceInfo {
    /// When 1, indicates that device firmware updates are not permitted
    /// while in CONFIG_LOCKED or RUN. When 0, indicates that firmware
    /// updates are permitted while in these states
    pub firmware_update_allowed: bool,

    /// TDI generates DMA requests without PASID
    pub generate_dma_without_pasid: bool,

    /// TDI generates DMA requests with PASID
    pub generate_dma_with_pasid: bool,

    /// ATS supported and enabled for the TDI
    pub ats_support_enabled: bool,

    /// PRS supported and enabled for the TDI
    pub prs_support_enabled: bool,
    #[bits(11)]
    _reserved0: u16,
}

/// PCI Express Base Specification Revision 6.3 Section 11.3.11 DEVICE_INTERFACE_REPORT
#[bitfield(u16)]
#[derive(KnownLayout, FromBytes, Immutable, IntoBytes)]
pub struct TdispTdiReportMmioFlags {
    /// MSI-X Table – if the range maps MSI-X table. This must be reported only if locked by the LOCK_INTERFACE_REQUEST.
    pub range_maps_msix_table: bool,

    /// MSI-X PBA – if the range maps MSI-X PBA. This must be reported only if locked by the LOCK_INTERFACE_REQUEST.
    pub range_maps_msix_pba: bool,

    /// IS_NON_TEE_MEM – must be 1b if the range is non-TEE memory.
    /// For attribute updatable ranges (see below), this field must indicate attribute of the range when the TDI was locked.
    pub is_non_tee_mem: bool,

    /// IS_MEM_ATTR_UPDATABLE – must be 1b if the attributes of this range is updatable using SET_MMIO_ATTRIBUTE_REQUEST
    pub is_mem_attr_updatable: bool,
    #[bits(12)]
    _reserved0: u16,
}

/// PCI Express Base Specification Revision 6.3 Section 11.3.11 DEVICE_INTERFACE_REPORT
#[derive(KnownLayout, FromBytes, Immutable, IntoBytes, Clone, Debug)]
#[repr(C)]
pub struct TdispTdiReportMmioInterfaceInfo {
    /// First 4K page with offset added
    pub first_4k_page_offset: u64,

    /// Number of 4K pages in this range
    pub num_4k_pages: u32,

    /// Range Attributes
    pub flags: TdispTdiReportMmioFlags,

    /// Range ID – a device specific identifier for the specified range.
    /// The range ID may be used to logically group one or more MMIO ranges into a larger range.
    pub range_id: u16,
}

static_assertions::const_assert_eq!(size_of::<TdispTdiReportMmioInterfaceInfo>(), 0x10);

/// PCI Express Base Specification Revision 6.3 Section 11.3.11 DEVICE_INTERFACE_REPORT
#[derive(KnownLayout, FromBytes, Immutable, IntoBytes, Debug)]
#[repr(C)]
struct TdiReportStructSerialized {
    pub interface_info: TdispTdiReportInterfaceInfo,
    _reserved0: u16,
    pub msi_x_message_control: u16,
    pub lnr_control: u16,
    pub tph_control: u32,
    pub mmio_range_count: u32,
    // Follows is a variable-sized # of `MmioInterfaceInfo` structs
    // based on the value of `mmio_range_count`, then a 4 byte
    // `device_specific_info_len`, then that many bytes of
    // vendor/device specific information.
}

static_assertions::const_assert_eq!(size_of::<TdiReportStructSerialized>(), 0x10);

/// Serialized size of the fixed portion of a TDI interface report, which is the
/// whole report for a TDI that claims no MMIO ranges.
pub const TDI_REPORT_HEADER_SIZE: usize = size_of::<TdiReportStructSerialized>();

/// The deserialized form of a TDI interface report.
#[derive(Debug)]
pub struct TdiReportStruct {
    /// Capabilities and DMA/ATS/PRS behavior the TDI reports for itself.
    pub interface_info: TdispTdiReportInterfaceInfo,

    /// MSI-X capability message control register state. Must be Clear if
    /// a) capability is not supported or b) MSI-X table is not locked
    pub msi_x_message_control: u16,

    /// LNR control register from LN Requester Extended Capability.
    /// Must be Clear if LNR capability is not supported. LN is deprecated in PCIe Revision 6.0.
    pub lnr_control: u16,

    /// TPH Requester Control Register from the TPH Requester Extended Capability.
    /// Must be Clear if a) TPH capability is not support or b) MSI-X table is not locked
    pub tph_control: u32,

    /// Each MMIO Range of the TDI is reported with the MMIO reporting offset added.
    /// Base and size in units of 4K pages
    pub mmio_interface_info: Vec<TdispTdiReportMmioInterfaceInfo>,

    /// Vendor/Device specific bytes that follow the MMIO range array, with the
    /// length prefix already consumed. Interpreted according to the
    /// vendor/device specific format. Empty if the device gives none, which
    /// includes a device that omits the length entirely.
    pub device_specific_info: Vec<u8>,
}

/// Reads a TDI interface report provided from the host into a struct.
pub fn deserialize_tdi_report(data: &[u8]) -> anyhow::Result<TdiReportStruct> {
    // Deserialize the static part of the report.
    let report_header = TdiReportStructSerialized::read_from_prefix(data)
        .map_err(|e| anyhow::anyhow!("failed to deserialize TDI report header: {e:?}"))?;
    let variable_portion_offset = report_header.1;
    let report = report_header.0;

    // Deserialize the variable portion of the report.
    let read_mmio_elems = <[TdispTdiReportMmioInterfaceInfo]>::ref_from_prefix_with_elems(
        variable_portion_offset,
        report.mmio_range_count as usize,
    )
    .map_err(|e| anyhow::anyhow!("failed to deserialize TDI report mmio_interface_info: {e:?}"))?;

    // The length and the device specific info follow the MMIO range array, so
    // they can only be read once the array's length is known. A device with
    // nothing to report may end the report after the ranges rather than
    // appending a zero length, so handle the case of absent trailer as not an error.
    let device_specific_info = if read_mmio_elems.1.is_empty() {
        Vec::new()
    } else {
        let (device_specific_info_len, trailing_bytes) = u32::read_from_prefix(read_mmio_elems.1)
            .map_err(|e| {
            anyhow::anyhow!("failed to deserialize DEVICE_SPECIFIC_INFO_LEN: {e:?}")
        })?;

        trailing_bytes
            .get(..device_specific_info_len as usize)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "TDI report claims {device_specific_info_len} bytes of device specific info, but only {} follow",
                    trailing_bytes.len()
                )
            })?
            .to_vec()
    };

    Ok(TdiReportStruct {
        interface_info: report.interface_info,
        msi_x_message_control: report.msi_x_message_control,
        lnr_control: report.lnr_control,
        tph_control: report.tph_control,
        mmio_interface_info: read_mmio_elems.0.to_vec(),
        device_specific_info,
    })
}

/// Writes a TDI interface report out in the form a host reports it, so that an
/// emulated device can describe the MMIO ranges it claims.
///
/// * `report` - The report to write out. Its MMIO ranges are written in the
///   order they appear, and their count is taken from the list rather than
///   being supplied separately.
pub fn serialize_tdi_report(report: &TdiReportStruct) -> Vec<u8> {
    let header = TdiReportStructSerialized {
        interface_info: report.interface_info,
        _reserved0: 0,
        msi_x_message_control: report.msi_x_message_control,
        lnr_control: report.lnr_control,
        tph_control: report.tph_control,
        mmio_range_count: report.mmio_interface_info.len() as u32,
    };

    let mut buffer = header.as_bytes().to_vec();
    for range in &report.mmio_interface_info {
        buffer.extend_from_slice(range.as_bytes());
    }
    buffer.extend_from_slice(&(report.device_specific_info.len() as u32).to_ne_bytes());
    buffer.extend_from_slice(&report.device_specific_info);
    buffer
}
