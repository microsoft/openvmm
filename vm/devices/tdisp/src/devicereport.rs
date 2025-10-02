use bitfield_struct::bitfield;
use zerocopy::{FromBytes, Immutable, KnownLayout};

/// Represents a type of report that can be requested from the TDI (VF).
#[derive(Debug)]
pub enum TdispTdiReport {
    /// Invalid report type. All usages of this report type should be treated as an error.
    Invalid,

    /// Guest requests the guest device ID of the TDI.
    GuestDeviceId,

    /// Guest requests the interface report of the TDI.
    InterfaceReport,
}

/// Represents a type of report that can be requested from the physical device.
#[derive(Debug)]
pub enum TdispDeviceReport {
    /// Invalid report type. All usages of this report type should be treated as an error.
    Invalid,

    /// Guest requests the certificate chain of the device.
    CertificateChain,

    /// Guest requests the measurements of the device.
    Measurements,

    /// Guest requests whether the device is registered.
    /// [TDISP TODO] Remove this report type? Doesn't seem to serve a purpose.
    IsRegistered,
}

impl From<&TdispTdiReport> for u32 {
    fn from(value: &TdispTdiReport) -> Self {
        match value {
            TdispTdiReport::Invalid => 0,
            TdispTdiReport::GuestDeviceId => 1,
            TdispTdiReport::InterfaceReport => 2,
        }
    }
}

/// Set to the number of enums in TdispTdiReport to assign an ID that is unique for this enum.
/// [TDISP TODO] Is there a better way to do this by calculating how many enums there are with Rust const types?
pub const TDISP_TDI_REPORT_ENUM_COUNT: u32 = 3;

impl From<&TdispDeviceReport> for u32 {
    fn from(value: &TdispDeviceReport) -> Self {
        match value {
            TdispDeviceReport::Invalid => TDISP_TDI_REPORT_ENUM_COUNT,
            TdispDeviceReport::CertificateChain => TDISP_TDI_REPORT_ENUM_COUNT + 1,
            TdispDeviceReport::Measurements => TDISP_TDI_REPORT_ENUM_COUNT + 2,
            TdispDeviceReport::IsRegistered => TDISP_TDI_REPORT_ENUM_COUNT + 3,
        }
    }
}

impl From<&TdispDeviceReportType> for u32 {
    fn from(value: &TdispDeviceReportType) -> Self {
        match value {
            TdispDeviceReportType::TdiReport(report_type) => report_type.into(),
            TdispDeviceReportType::DeviceReport(report_type) => report_type.into(),
        }
    }
}

impl From<u32> for TdispDeviceReportType {
    fn from(value: u32) -> Self {
        match value {
            0 => TdispDeviceReportType::TdiReport(TdispTdiReport::Invalid),
            1 => TdispDeviceReportType::TdiReport(TdispTdiReport::GuestDeviceId),
            2 => TdispDeviceReportType::TdiReport(TdispTdiReport::InterfaceReport),
            3 => TdispDeviceReportType::DeviceReport(TdispDeviceReport::Invalid),
            4 => TdispDeviceReportType::DeviceReport(TdispDeviceReport::CertificateChain),
            5 => TdispDeviceReportType::DeviceReport(TdispDeviceReport::Measurements),
            6 => TdispDeviceReportType::DeviceReport(TdispDeviceReport::IsRegistered),
            _ => TdispDeviceReportType::TdiReport(TdispTdiReport::Invalid),
        }
    }
}

/// Represents a type of report that can be requested from an assigned TDISP device.
#[derive(Debug)]
pub enum TdispDeviceReportType {
    /// A report produced by the device interface and not the physical interface.
    TdiReport(TdispTdiReport),

    /// A report produced by the physical interface and not the device interface.
    DeviceReport(TdispDeviceReport),
}

/// PCI Express Base Specification Revision 6.3 Section 11.3.11 DEVICE_INTERFACE_REPORT
#[bitfield(u16)]
#[derive(KnownLayout, FromBytes, Immutable)]
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
#[derive(KnownLayout, FromBytes, Immutable)]
pub struct TdispTdiReportMmioFlags {
    /// MSI-X Table – if the range maps MSI-X table. This must be reported only if locked by the LOCK_INTERFACE_REQUEST.
    pub range_maps_msix_table: bool,

    /// MSI-X PBA – if the range maps MSI-X PBA. This must be reported only if locked by the LOCK_INTERFACE_REQUEST.
    pub range_maps_msix_pba: bool,

    /// IS_NON_TEE_MEM – must be 1b if the range is non-TEE memory.
    /// For attribute updatable ranges (see below), this field must indicate attribute of the range when the TDI was locked.
    pub is_non_tee_mem: bool,

    ///  IS_MEM_ATTR_UPDATABLE – must be 1b if the attributes of this range is updatable using SET_MMIO_ATTRIBUTE_REQUEST
    pub is_mem_attr_updatable: bool,
    #[bits(12)]
    _reserved0: u16,
}

/// PCI Express Base Specification Revision 6.3 Section 11.3.11 DEVICE_INTERFACE_REPORT
#[derive(KnownLayout, FromBytes, Immutable, Clone, Debug)]
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
#[derive(KnownLayout, FromBytes, Immutable, Debug)]
#[repr(C)]
struct TdiReportStructSerialized {
    pub interface_info: TdispTdiReportInterfaceInfo,
    _reserved0: u16,
    pub msi_x_message_control: u16,
    pub lnr_control: u16,
    pub tph_control: u32,
    pub mmio_range_count: u32,
    // Follows is a variable-sized # of `MmioInterfaceInfo` structs
    // based on the value of `mmio_range_count`.
}

static_assertions::const_assert_eq!(size_of::<TdiReportStructSerialized>(), 0x10);

/// The deserialized form of a TDI interface report.
#[derive(Debug)]
pub struct TdiReportStruct {
    /// See: `TdispTdiReportInterfaceInfo`
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

    // [TDISP TODO] Parse the vendor specific info
    let _vendor_specific_info = read_mmio_elems.1.to_vec();

    Ok(TdiReportStruct {
        interface_info: report.interface_info,
        msi_x_message_control: report.msi_x_message_control,
        lnr_control: report.lnr_control,
        tph_control: report.tph_control,
        mmio_interface_info: read_mmio_elems.0.to_vec(),
    })
}
