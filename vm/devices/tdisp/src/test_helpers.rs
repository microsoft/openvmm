// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::TdispHostDeviceInterface;
use crate::TdispHostStateMachine;
use parking_lot::Mutex;
use std::sync::Arc;
use tdisp_proto::TdispDeviceInterfaceInfo;
use tdisp_proto::TdispGuestProtocolType;
use tdisp_proto::TdispReportType;

/// Implements the host side of the TDISP interface for the mock NullDevice.
pub struct NullTdispHostInterface {}
impl TdispHostDeviceInterface for NullTdispHostInterface {
    fn tdisp_negotiate_protocol(
        &mut self,
        _requested_guest_protocol: TdispGuestProtocolType,
    ) -> anyhow::Result<TdispDeviceInterfaceInfo> {
        Ok(TdispDeviceInterfaceInfo {
            guest_protocol_type: TdispGuestProtocolType::AmdSevTioV10 as i32,
            supported_features: 0xDEAD,
            tdisp_device_id: 99,
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
pub fn make_null_tdisp_interface() -> TdispHostStateMachine {
    TdispHostStateMachine::new(Arc::new(Mutex::new(NullTdispHostInterface {})))
}
