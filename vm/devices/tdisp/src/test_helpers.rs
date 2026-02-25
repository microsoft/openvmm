// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::TdispHostDeviceInterface;
use crate::TdispHostDeviceTargetEmulator;
use parking_lot::Mutex;
use std::sync::Arc;
use tdisp_proto::TdispDeviceInterfaceInfo;
use tdisp_proto::TdispGuestProtocolType;
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

    fn tdisp_get_device_report(
        &mut self,
        _report_type: TdispReportType,
    ) -> anyhow::Result<Vec<u8>> {
        Ok(vec![])
    }
}

/// Implements the host side of the TDISP interface for a mock device that does nothing.
pub fn make_null_tdisp_interface(debug_device_id: &str) -> TdispHostDeviceTargetEmulator {
    TdispHostDeviceTargetEmulator::new(
        Arc::new(Mutex::new(NullTdispHostInterface {})),
        debug_device_id,
    )
}
