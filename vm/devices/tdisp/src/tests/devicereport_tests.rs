// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unit tests for reading and writing the TDI interface report.

use crate::devicereport::TDI_REPORT_HEADER_SIZE;
use crate::devicereport::TdiReportStruct;
use crate::devicereport::TdispTdiReportInterfaceInfo;
use crate::devicereport::TdispTdiReportMmioFlags;
use crate::devicereport::TdispTdiReportMmioInterfaceInfo;
use crate::devicereport::deserialize_tdi_report;
use crate::devicereport::serialize_tdi_report;

/// A report from a TDI with two MMIO ranges: a 64K range of TEE memory and a
/// 4K range mapping the MSI-X table.
fn two_range_report() -> TdiReportStruct {
    TdiReportStruct {
        interface_info: TdispTdiReportInterfaceInfo::new().with_generate_dma_without_pasid(true),
        msi_x_message_control: 0x8001,
        lnr_control: 0x1234,
        tph_control: 0xabcd_0123,
        mmio_interface_info: vec![
            TdispTdiReportMmioInterfaceInfo {
                first_4k_page_offset: 0xf000_0000,
                num_4k_pages: 16,
                flags: TdispTdiReportMmioFlags::new(),
                range_id: 0,
            },
            TdispTdiReportMmioInterfaceInfo {
                first_4k_page_offset: 0xf001_0000,
                num_4k_pages: 1,
                flags: TdispTdiReportMmioFlags::new()
                    .with_range_maps_msix_table(true)
                    .with_is_non_tee_mem(true),
                range_id: 4,
            },
        ],
    }
}

#[test]
fn report_survives_a_round_trip() {
    let report = two_range_report();
    let parsed = deserialize_tdi_report(&serialize_tdi_report(&report)).unwrap();

    assert_eq!(
        parsed.interface_info.into_bits(),
        report.interface_info.into_bits()
    );
    assert_eq!(parsed.msi_x_message_control, report.msi_x_message_control);
    assert_eq!(parsed.lnr_control, report.lnr_control);
    assert_eq!(parsed.tph_control, report.tph_control);
    assert_eq!(parsed.mmio_interface_info.len(), 2);
    for (parsed, original) in parsed
        .mmio_interface_info
        .iter()
        .zip(report.mmio_interface_info.iter())
    {
        assert_eq!(parsed.first_4k_page_offset, original.first_4k_page_offset);
        assert_eq!(parsed.num_4k_pages, original.num_4k_pages);
        assert_eq!(parsed.flags.into_bits(), original.flags.into_bits());
        assert_eq!(parsed.range_id, original.range_id);
    }
}

#[test]
fn range_count_comes_from_the_range_list() {
    let report = two_range_report();
    let buffer = serialize_tdi_report(&report);

    // The header, then one entry per range.
    assert_eq!(
        buffer.len(),
        TDI_REPORT_HEADER_SIZE + 2 * size_of::<TdispTdiReportMmioInterfaceInfo>()
    );
}

#[test]
fn a_report_with_no_ranges_is_just_the_header() {
    let report = TdiReportStruct {
        interface_info: TdispTdiReportInterfaceInfo::new(),
        msi_x_message_control: 0,
        lnr_control: 0,
        tph_control: 0,
        mmio_interface_info: Vec::new(),
    };

    let buffer = serialize_tdi_report(&report);
    assert_eq!(buffer.len(), TDI_REPORT_HEADER_SIZE);
    assert!(
        deserialize_tdi_report(&buffer)
            .unwrap()
            .mmio_interface_info
            .is_empty()
    );
}
