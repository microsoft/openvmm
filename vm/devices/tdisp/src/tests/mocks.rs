// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::TdispGuestRequestInterface;
use crate::TdispHostDeviceInterface;
use crate::TdispHostDeviceTargetEmulator;
use crate::TdispHostStateMachine;
use crate::test_helpers::TDISP_MOCK_DEVICE_ID;
use crate::test_helpers::TDISP_MOCK_GUEST_PROTOCOL;
use crate::test_helpers::TDISP_MOCK_SUPPORTED_FEATURES;
use parking_lot::Mutex;
use std::sync::Arc;
use tdisp_proto::TdispDeviceInterfaceInfo;
use tdisp_proto::TdispGuestProtocolType;
use tdisp_proto::TdispReportType;

#[derive(Debug, PartialEq, Clone)]
pub enum LastCall {
    NegotiateProtocol,
    BindDevice,
    StartDevice,
    UnbindDevice,
    GetDeviceReport(TdispReportType),
}

pub struct TrackingHostInterface {
    last_call: Arc<Mutex<Option<LastCall>>>,
    report_buffer: Arc<Mutex<Vec<u8>>>,
}

impl TdispHostDeviceInterface for TrackingHostInterface {
    fn tdisp_bind_device(&mut self) -> anyhow::Result<()> {
        *self.last_call.lock() = Some(LastCall::BindDevice);
        Ok(())
    }

    fn tdisp_start_device(&mut self) -> anyhow::Result<()> {
        *self.last_call.lock() = Some(LastCall::StartDevice);
        Ok(())
    }

    fn tdisp_unbind_device(&mut self) -> anyhow::Result<()> {
        *self.last_call.lock() = Some(LastCall::UnbindDevice);
        Ok(())
    }

    /// Returns a mock report buffer that is configurable.
    fn tdisp_get_device_report(&mut self, report_type: TdispReportType) -> anyhow::Result<Vec<u8>> {
        if report_type == TdispReportType::InterfaceReport {
            *self.last_call.lock() = Some(LastCall::GetDeviceReport(report_type));
            Ok(self.report_buffer.lock().clone())
        } else {
            *self.last_call.lock() = Some(LastCall::GetDeviceReport(report_type));
            Err(anyhow::anyhow!(
                "mock test checks only that InterfaceReport is requested"
            ))
        }
    }

    fn tdisp_negotiate_protocol(
        &mut self,
        _requested_guest_protocol: TdispGuestProtocolType,
    ) -> anyhow::Result<TdispDeviceInterfaceInfo> {
        *self.last_call.lock() = Some(LastCall::NegotiateProtocol);
        Ok(TdispDeviceInterfaceInfo {
            guest_protocol_type: TDISP_MOCK_GUEST_PROTOCOL as i32,
            supported_features: TDISP_MOCK_SUPPORTED_FEATURES,
            tdisp_device_id: TDISP_MOCK_DEVICE_ID,
        })
    }
}

/// Mock host emulator that records calls and provides a report buffer that is configurable.
pub struct MockHostEmulator {
    pub emulator: TdispHostDeviceTargetEmulator,
    pub last_call: Arc<Mutex<Option<LastCall>>>,
    pub report_buffer: Arc<Mutex<Vec<u8>>>,
}

pub fn new_emulator() -> MockHostEmulator {
    let last_call: Arc<Mutex<Option<LastCall>>> = Arc::new(Mutex::new(None));
    let report_buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(vec![0xDE, 0xAD, 0xBE, 0xEF]));
    let interface = TrackingHostInterface {
        last_call: last_call.clone(),
        report_buffer: report_buffer.clone(),
    };
    let emulator =
        TdispHostDeviceTargetEmulator::new(Arc::new(Mutex::new(interface)), "test-device");
    MockHostEmulator {
        emulator,
        last_call,
        report_buffer,
    }
}

pub struct MockTdiStateMachine {
    pub machine: TdispHostStateMachine,
    pub last_call: Arc<Mutex<Option<LastCall>>>,
}

/// Returns a fresh state machine paired with a handle for inspecting which
/// host-interface method was called most recently.
pub fn new_machine() -> MockTdiStateMachine {
    let last_call: Arc<Mutex<Option<LastCall>>> = Arc::new(Mutex::new(None));
    let report_buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(vec![0xDE, 0xAD, 0xBE, 0xEF]));
    let interface = TrackingHostInterface {
        last_call: last_call.clone(),
        report_buffer: report_buffer.clone(),
    };

    let mut machine = TdispHostStateMachine::new(Arc::new(Mutex::new(interface)));

    // Forcibly negotiate any protocol to avoid the need for a test to do it.
    // This otherwise doesn't affect test behavior right now.
    machine
        .tdisp_negotiate_protocol(TDISP_MOCK_GUEST_PROTOCOL)
        .unwrap();

    // Reset last_call to avoid interference from the emulator.
    *last_call.lock() = None;

    MockTdiStateMachine { machine, last_call }
}
