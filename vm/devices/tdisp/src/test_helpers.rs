// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::TdispHostDeviceInterface;
use crate::TdispHostDeviceTargetEmulator;
use crate::devicereport::TDI_REPORT_HEADER_SIZE;
use parking_lot::Mutex;
use std::sync::Arc;
use tdisp_proto::TdispDeviceInterfaceInfo;
use tdisp_proto::TdispGuestProtocolType;
use tdisp_proto::TdispMmioRangeAction;
use tdisp_proto::TdispReportType;

/// Guest protocol that will be negotiated by the mock device.
pub const TDISP_MOCK_GUEST_PROTOCOL: TdispGuestProtocolType = TdispGuestProtocolType::AmdSevTioV1;

/// Device features that will be negotiated by the mock device.
pub const TDISP_MOCK_SUPPORTED_FEATURES: u64 = 0xDEAD;

/// Device ID that will be negotiated by the mock device.
pub const TDISP_MOCK_DEVICE_ID: u64 = 99;

/// Implements the host side of the TDISP interface for the mock NullDevice.
pub struct NullTdispHostInterface {}
impl TdispHostDeviceInterface for NullTdispHostInterface {
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
        Ok(())
    }

    fn tdisp_get_device_report(&mut self, report_type: TdispReportType) -> anyhow::Result<Vec<u8>> {
        match report_type {
            // The wire format is a little-endian u64.
            TdispReportType::GuestDeviceId => Ok(TDISP_MOCK_DEVICE_ID.to_le_bytes().to_vec()),
            // A TDI that claims no MMIO ranges, so the report is the fixed
            // header with a range count of zero and nothing following it.
            TdispReportType::InterfaceReport => Ok(vec![0; TDI_REPORT_HEADER_SIZE]),
            other => anyhow::bail!("the mock device has no {other:?} report to give"),
        }
    }

    fn tdisp_modify_mmio_range(
        &mut self,
        _action: TdispMmioRangeAction,
        _range_id: u16,
        _gpa_base: u64,
        _range_len_bytes: u64,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Implements the host side of the TDISP interface for a mock device that does nothing.
pub fn new_null_tdisp_interface(debug_device_id: &str) -> TdispHostDeviceTargetEmulator {
    TdispHostDeviceTargetEmulator::new(
        Arc::new(Mutex::new(NullTdispHostInterface {})),
        debug_device_id,
    )
}
