// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The host side of TDISP for the fault controller, which OpenVMM repurposes as
//! an emulated TDISP device.
//!
//! Describes the controller's own BARs in the TDI interface report and records
//! which of those ranges TDISP has unblocked, so that the controller can refuse
//! access to a range the guest has not yet attested and accepted.

use crate::BAR0_LEN;
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::Arc;
use tdisp::TdispDeviceInterfaceInfo;
use tdisp::TdispGuestProtocolType;
use tdisp::TdispHostDeviceInterface;
use tdisp::TdispHostDeviceTargetEmulator;
use tdisp::TdispMmioRangeAction;
use tdisp::TdispReportType;
use tdisp::devicereport::TdiReportStruct;
use tdisp::devicereport::TdispTdiReportInterfaceInfo;
use tdisp::devicereport::TdispTdiReportMmioFlags;
use tdisp::devicereport::TdispTdiReportMmioInterfaceInfo;
use tdisp::devicereport::serialize_tdi_report;
use tdisp::test_helpers::TDISP_MOCK_DEVICE_ID;
use tdisp::test_helpers::TDISP_MOCK_GUEST_PROTOCOL;
use tdisp::test_helpers::TDISP_MOCK_SUPPORTED_FEATURES;

/// The range id the controller's register BAR is reported under, which is also
/// its BAR index.
pub(crate) const BAR0_RANGE_ID: u16 = 0;

/// The range id the MSI-X table and PBA BAR is reported under, which is also
/// its BAR index.
const MSIX_RANGE_ID: u16 = 4;

/// The page size MMIO ranges are reported in.
const REPORT_PAGE_SIZE: u64 = 0x1000;

/// Which of the device's MMIO ranges TDISP currently allows the guest to reach.
///
/// Shared between the host TDISP interface, which is told when a range is
/// unblocked or blocked, and the controller, which honors it on every MMIO
/// access. A range starts blocked and stays that way until the guest has
/// attested the TDI and accepted the range into its context, and goes back to
/// blocked when the TDI is unbound.
#[derive(Clone, Default)]
pub struct TdispMmioRanges(Arc<Mutex<HashSet<u16>>>);

impl TdispMmioRanges {
    /// Whether the guest may currently reach `range_id`.
    ///
    /// * `range_id` - The range to check, which for this device is the BAR
    ///   index.
    pub fn is_unblocked(&self, range_id: u16) -> bool {
        self.0.lock().contains(&range_id)
    }
}

/// Builds the TDISP host target the fault controller exposes to a guest, along
/// with the record of unblocked ranges the controller reads on every MMIO
/// access.
///
/// * `debug_device_id` - Identifies this device in TDISP traces.
/// * `msix_bar_len` - Length in bytes of the MSI-X BAR, which the interface
///   report describes as a non-TEE range.
pub(crate) fn new_tdisp_interface(
    debug_device_id: &str,
    msix_bar_len: u64,
) -> (TdispHostDeviceTargetEmulator, TdispMmioRanges) {
    let ranges = TdispMmioRanges::default();
    let emulator = TdispHostDeviceTargetEmulator::new(
        Arc::new(Mutex::new(FaultControllerTdispInterface {
            ranges: ranges.clone(),
            msix_bar_len,
        })),
        debug_device_id,
    );
    (emulator, ranges)
}

/// The platform actions a real TDISP host would perform, emulated for the fault
/// controller.
struct FaultControllerTdispInterface {
    ranges: TdispMmioRanges,
    msix_bar_len: u64,
}

impl FaultControllerTdispInterface {
    /// The report the device gives for itself, describing the two BARs it
    /// implements.
    fn interface_report(&self) -> TdiReportStruct {
        TdiReportStruct {
            interface_info: TdispTdiReportInterfaceInfo::new(),
            msi_x_message_control: 0,
            lnr_control: 0,
            tph_control: 0,
            mmio_interface_info: vec![
                // The register BAR is TEE memory, so it only becomes reachable
                // once the guest has attested the TDI and accepted the range.
                TdispTdiReportMmioInterfaceInfo {
                    first_4k_page_offset: 0,
                    num_4k_pages: (BAR0_LEN / REPORT_PAGE_SIZE) as u32,
                    flags: TdispTdiReportMmioFlags::new(),
                    range_id: BAR0_RANGE_ID,
                },
                // The MSI-X table and PBA are emulated by the host and have no
                // guest-private backing, so they are reported as non-TEE memory
                // and stay reachable throughout.
                TdispTdiReportMmioInterfaceInfo {
                    first_4k_page_offset: 0,
                    num_4k_pages: self.msix_bar_len.div_ceil(REPORT_PAGE_SIZE) as u32,
                    flags: TdispTdiReportMmioFlags::new()
                        .with_range_maps_msix_table(true)
                        .with_range_maps_msix_pba(true)
                        .with_is_non_tee_mem(true),
                    range_id: MSIX_RANGE_ID,
                },
            ],
        }
    }
}

impl TdispHostDeviceInterface for FaultControllerTdispInterface {
    fn tdisp_negotiate_protocol(
        &mut self,
        _requested_guest_protocol: TdispGuestProtocolType,
    ) -> anyhow::Result<TdispDeviceInterfaceInfo> {
        Ok(TdispDeviceInterfaceInfo {
            guest_protocol_type: TDISP_MOCK_GUEST_PROTOCOL as i32,
            supported_features: TDISP_MOCK_SUPPORTED_FEATURES,
            tdisp_device_id: TDISP_MOCK_DEVICE_ID,
        })
    }

    fn tdisp_bind_device(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn tdisp_start_device(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn tdisp_unbind_device(&mut self) -> anyhow::Result<()> {
        // Every range the guest had accepted goes away with the binding, so
        // the device stops answering on all of them.
        let mut ranges = self.ranges.0.lock();
        tracing::info!(
            unblocked_ranges = ranges.len(),
            "fault controller TDISP unbind, blocking every MMIO range"
        );
        ranges.clear();
        Ok(())
    }

    fn tdisp_get_device_report(&mut self, report_type: TdispReportType) -> anyhow::Result<Vec<u8>> {
        match report_type {
            // The wire format is a little-endian u64.
            TdispReportType::GuestDeviceId => Ok(TDISP_MOCK_DEVICE_ID.to_le_bytes().to_vec()),
            TdispReportType::InterfaceReport => Ok(serialize_tdi_report(&self.interface_report())),
            other => anyhow::bail!("the fault controller has no {other:?} report to give"),
        }
    }

    fn tdisp_modify_mmio_range(
        &mut self,
        action: TdispMmioRangeAction,
        range_id: u16,
        gpa_base: u64,
        range_len_bytes: u64,
    ) -> anyhow::Result<()> {
        tracing::info!(
            ?action,
            range_id,
            gpa_base,
            range_len_bytes,
            "fault controller TDISP MMIO range change"
        );

        match action {
            TdispMmioRangeAction::UnblockMmioRange => {
                self.ranges.0.lock().insert(range_id);
            }
            TdispMmioRangeAction::BlockMmioRange => {
                self.ranges.0.lock().remove(&range_id);
            }
            TdispMmioRangeAction::Invalid => {
                anyhow::bail!("invalid MMIO range action for range {range_id}")
            }
        }

        Ok(())
    }
}
