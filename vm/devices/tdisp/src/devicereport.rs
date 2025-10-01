use bitfield_struct::bitfield;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

#[bitfield(u16)]
#[derive(KnownLayout, FromBytes, Immutable)]
pub struct TdispTdiReportInterfaceInfo {
    pub firmware_update_allowed: bool,
    pub generate_dma_without_pasid: bool,
    pub generate_dma_with_pasid: bool,
    pub ats_support_enabled: bool,
    pub prs_support_enabled: bool,
    #[bits(11)]
    _reserved0: u16,
}

#[bitfield(u16)]
#[derive(KnownLayout, FromBytes, Immutable)]
pub struct TdispTdiReportMmioFlags {
    pub range_maps_msix_table: bool,
    pub range_maps_msix_pba: bool,
    pub is_non_tee_mem: bool,
    pub is_mem_attr_updatable: bool,
    #[bits(12)]
    _reserved0: u16,
}

#[derive(KnownLayout, FromBytes, Immutable, Clone, Debug)]
pub struct TdispTdiReportMmioInterfaceInfo {
    pub first_4k_page_offset: u64,
    pub num_4k_pages: u32,
    pub flags: TdispTdiReportMmioFlags,
    pub range_id: u16,
}

static_assertions::const_assert_eq!(size_of::<TdispTdiReportMmioInterfaceInfo>(), 0x10);

#[derive(KnownLayout, FromBytes, Immutable, Debug)]
#[repr(C)]
struct TdiReportStructSerialized {
    pub interface_info: TdispTdiReportInterfaceInfo,
    pub _reserved0: u16,
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
    pub interface_info: TdispTdiReportInterfaceInfo,
    pub msi_x_message_control: u16,
    pub lnr_control: u16,
    pub tph_control: u32,
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
