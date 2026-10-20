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
        device_specific_info: Vec::new(),
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

    // The header, one entry per range, then the device specific info length.
    assert_eq!(
        buffer.len(),
        TDI_REPORT_HEADER_SIZE
            + 2 * size_of::<TdispTdiReportMmioInterfaceInfo>()
            + size_of::<u32>()
    );
}

#[test]
fn a_report_with_no_ranges_is_the_header_and_a_zero_length() {
    let report = TdiReportStruct {
        interface_info: TdispTdiReportInterfaceInfo::new(),
        msi_x_message_control: 0,
        lnr_control: 0,
        tph_control: 0,
        mmio_interface_info: Vec::new(),
        device_specific_info: Vec::new(),
    };

    let buffer = serialize_tdi_report(&report);
    // The header, no ranges, then a device specific info length of zero.
    assert_eq!(buffer.len(), TDI_REPORT_HEADER_SIZE + size_of::<u32>());
    assert!(
        deserialize_tdi_report(&buffer)
            .unwrap()
            .mmio_interface_info
            .is_empty()
    );
}

#[test]
fn device_specific_info_survives_a_round_trip() {
    let mut report = two_range_report();
    report.device_specific_info = vec![0xde, 0xad, 0xbe, 0xef, 0x00, 0x00];

    let parsed = deserialize_tdi_report(&serialize_tdi_report(&report)).unwrap();
    assert_eq!(parsed.device_specific_info, report.device_specific_info);
}

#[test]
fn a_report_ending_after_the_ranges_has_no_device_specific_info() {
    let report = two_range_report();
    let mut buffer = serialize_tdi_report(&report);

    // Not every device appends the length, so a report may end here.
    buffer.truncate(buffer.len() - size_of::<u32>());

    let parsed = deserialize_tdi_report(&buffer).unwrap();
    assert_eq!(parsed.mmio_interface_info.len(), 2);
    assert!(parsed.device_specific_info.is_empty());
}

#[test]
fn a_device_specific_info_length_past_the_end_is_rejected() {
    let report = two_range_report();
    let mut buffer = serialize_tdi_report(&report);

    // Claim eight bytes of device specific info without supplying any.
    let len_offset = buffer.len() - size_of::<u32>();
    buffer[len_offset..].copy_from_slice(&8u32.to_ne_bytes());

    assert!(deserialize_tdi_report(&buffer).is_err());
}
