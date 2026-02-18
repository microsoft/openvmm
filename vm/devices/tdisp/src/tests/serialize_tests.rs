// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unit tests for serialization and deserialization of TDISP guest-to-host commands and responses.
//! These are fairly basic and not exhaustive to all packet types. Basically ensures the most core
//! serialization and deserialization logic is working.

use tdisp_proto::{
    GuestToHostCommand, GuestToHostResponse, TdispCommandRequestBind,
    TdispCommandRequestGetDeviceInterfaceInfo, TdispCommandRequestGetTdiReport,
    TdispCommandRequestStartTdi, TdispCommandRequestUnbind, TdispCommandResponseBind,
    TdispCommandResponseGetDeviceInterfaceInfo, TdispCommandResponseGetTdiReport,
    TdispCommandResponseStartTdi, TdispCommandResponseUnbind, TdispDeviceInterfaceInfo,
    TdispGuestOperationErrorCode, TdispGuestUnbindReason, TdispReportType, TdispTdiState,
    guest_to_host_command::Command, guest_to_host_response::Response,
};

use crate::serialize_proto::{
    deserialize_command, deserialize_response, serialize_command, serialize_response,
};

/// Build a well-formed response wrapper around the given `Response` variant.
fn make_response(response: Response) -> GuestToHostResponse {
    GuestToHostResponse {
        result: TdispGuestOperationErrorCode::Success as i32,
        tdi_state_before: TdispTdiState::Unlocked as i32,
        tdi_state_after: TdispTdiState::Locked as i32,
        response: Some(response),
    }
}

// ── Command round-trip tests ──────────────────────────────────────────────────

#[test]
fn test_command_get_device_interface_info_roundtrip() {
    let cmd = GuestToHostCommand {
        device_id: 42,
        command: Some(Command::GetDeviceInterfaceInfo(
            TdispCommandRequestGetDeviceInterfaceInfo {},
        )),
    };
    let bytes = serialize_command(&cmd);
    let got = deserialize_command(&bytes).unwrap();
    assert_eq!(got.device_id, 42);
    assert!(matches!(
        got.command,
        Some(Command::GetDeviceInterfaceInfo(_))
    ));
}

#[test]
fn test_command_bind_roundtrip() {
    let cmd = GuestToHostCommand {
        device_id: 1,
        command: Some(Command::Bind(TdispCommandRequestBind {})),
    };
    let bytes = serialize_command(&cmd);
    let got = deserialize_command(&bytes).unwrap();
    assert_eq!(got.device_id, 1);
    assert!(matches!(got.command, Some(Command::Bind(_))));
}

#[test]
fn test_command_start_tdi_roundtrip() {
    let cmd = GuestToHostCommand {
        device_id: 7,
        command: Some(Command::StartTdi(TdispCommandRequestStartTdi {})),
    };
    let bytes = serialize_command(&cmd);
    let got = deserialize_command(&bytes).unwrap();
    assert_eq!(got.device_id, 7);
    assert!(matches!(got.command, Some(Command::StartTdi(_))));
}

#[test]
fn test_command_get_tdi_report_roundtrip() {
    let cmd = GuestToHostCommand {
        device_id: 100,
        command: Some(Command::GetTdiReport(TdispCommandRequestGetTdiReport {
            report_type: TdispReportType::GuestDeviceId as i32,
        })),
    };
    let bytes = serialize_command(&cmd);
    let got = deserialize_command(&bytes).unwrap();
    assert_eq!(got.device_id, 100);
    let Some(Command::GetTdiReport(req)) = got.command else {
        panic!("expected GetTdiReport command");
    };
    assert_eq!(req.report_type, TdispReportType::GuestDeviceId as i32);
}

#[test]
fn test_command_unbind_roundtrip() {
    let cmd = GuestToHostCommand {
        device_id: 5,
        command: Some(Command::Unbind(TdispCommandRequestUnbind {
            unbind_reason: TdispGuestUnbindReason::Graceful as i32,
        })),
    };
    let bytes = serialize_command(&cmd);
    let got = deserialize_command(&bytes).unwrap();
    assert_eq!(got.device_id, 5);
    let Some(Command::Unbind(req)) = got.command else {
        panic!("expected Unbind command");
    };
    assert_eq!(req.unbind_reason, TdispGuestUnbindReason::Graceful as i32);
}

// ── Command validation-failure tests ─────────────────────────────────────────

#[test]
fn test_deserialize_command_rejects_missing_command_field() {
    // A GuestToHostCommand with no oneof variant set must be rejected.
    let cmd = GuestToHostCommand {
        device_id: 1,
        command: None,
    };
    let bytes = serialize_command(&cmd);
    assert!(deserialize_command(&bytes).is_err());
}

#[test]
fn test_deserialize_command_rejects_malformed_bytes() {
    // 0x80 is the start of an incomplete varint; prost must reject it.
    assert!(deserialize_command(&[0x80]).is_err());
}

// ── Response round-trip tests ─────────────────────────────────────────────────

#[test]
fn test_response_bind_roundtrip() {
    let resp = make_response(Response::Bind(TdispCommandResponseBind {}));
    let bytes = serialize_response(&resp);
    let got = deserialize_response(&bytes).unwrap();
    assert_eq!(got.result, TdispGuestOperationErrorCode::Success as i32);
    assert_eq!(got.tdi_state_before, TdispTdiState::Unlocked as i32);
    assert_eq!(got.tdi_state_after, TdispTdiState::Locked as i32);
    assert!(matches!(got.response, Some(Response::Bind(_))));
}

#[test]
fn test_response_start_tdi_roundtrip() {
    let resp = make_response(Response::StartTdi(TdispCommandResponseStartTdi {}));
    let bytes = serialize_response(&resp);
    let got = deserialize_response(&bytes).unwrap();
    assert!(matches!(got.response, Some(Response::StartTdi(_))));
}

#[test]
fn test_response_unbind_roundtrip() {
    let resp = make_response(Response::Unbind(TdispCommandResponseUnbind {}));
    let bytes = serialize_response(&resp);
    let got = deserialize_response(&bytes).unwrap();
    assert!(matches!(got.response, Some(Response::Unbind(_))));
}

#[test]
fn test_response_get_device_interface_info_roundtrip() {
    let resp = make_response(Response::GetDeviceInterfaceInfo(
        TdispCommandResponseGetDeviceInterfaceInfo {
            interface_info: Some(TdispDeviceInterfaceInfo {
                interface_version_major: 1,
                interface_version_minor: 0,
                supported_features: 0xDEAD,
                tdisp_device_id: 99,
            }),
        },
    ));
    let bytes = serialize_response(&resp);
    let got = deserialize_response(&bytes).unwrap();
    let Some(Response::GetDeviceInterfaceInfo(r)) = got.response else {
        panic!("expected GetDeviceInterfaceInfo response");
    };
    let info = r.interface_info.unwrap();
    assert_eq!(info.interface_version_major, 1);
    assert_eq!(info.interface_version_minor, 0);
    assert_eq!(info.supported_features, 0xDEAD);
    assert_eq!(info.tdisp_device_id, 99);
}

#[test]
fn test_response_get_tdi_report_roundtrip() {
    let report_data = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let resp = make_response(Response::GetTdiReport(TdispCommandResponseGetTdiReport {
        report_type: TdispReportType::InterfaceReport as i32,
        report_buffer: report_data.clone(),
    }));
    let bytes = serialize_response(&resp);
    let got = deserialize_response(&bytes).unwrap();
    let Some(Response::GetTdiReport(r)) = got.response else {
        panic!("expected GetTdiReport response");
    };
    assert_eq!(r.report_type, TdispReportType::InterfaceReport as i32);
    assert_eq!(r.report_buffer, report_data);
}

// ── Response validation-failure tests ────────────────────────────────────────

#[test]
fn test_deserialize_response_rejects_missing_response_field() {
    let resp = GuestToHostResponse {
        result: TdispGuestOperationErrorCode::Success as i32,
        tdi_state_before: TdispTdiState::Unlocked as i32,
        tdi_state_after: TdispTdiState::Locked as i32,
        response: None,
    };
    let bytes = serialize_response(&resp);
    assert!(deserialize_response(&bytes).is_err());
}

#[test]
fn test_deserialize_response_rejects_empty_report_buffer() {
    // GetTdiReport with an empty report_buffer must be rejected by validation.
    let resp = make_response(Response::GetTdiReport(TdispCommandResponseGetTdiReport {
        report_type: TdispReportType::InterfaceReport as i32,
        report_buffer: vec![],
    }));
    let bytes = serialize_response(&resp);
    assert!(deserialize_response(&bytes).is_err());
}

#[test]
fn test_deserialize_response_rejects_missing_interface_info() {
    // GetDeviceInterfaceInfo response with interface_info = None must be rejected.
    let resp = make_response(Response::GetDeviceInterfaceInfo(
        TdispCommandResponseGetDeviceInterfaceInfo {
            interface_info: None,
        },
    ));
    let bytes = serialize_response(&resp);
    assert!(deserialize_response(&bytes).is_err());
}

#[test]
fn test_deserialize_response_rejects_malformed_bytes() {
    assert!(deserialize_response(&[0x80]).is_err());
}
