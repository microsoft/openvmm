// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use prost::Message;
use test_with_tracing::test;

fn variants() -> [(Response, &'static str); 6] {
    [
        (
            Response::GetDeviceInterfaceInfo(TdispCommandResponseGetDeviceInterfaceInfo {
                interface_info: Some(TdispDeviceInterfaceInfo {
                    guest_protocol_type: 2,
                    supported_features: 0x8877665544332211,
                    tdisp_device_id: 0x123456789,
                }),
            }),
            "GetDeviceInterfaceInfo",
        ),
        (Response::Bind(TdispCommandResponseBind {}), "Bind"),
        (
            Response::StartTdi(TdispCommandResponseStartTdi {}),
            "StartTdi",
        ),
        (Response::Unbind(TdispCommandResponseUnbind {}), "Unbind"),
        (
            Response::GetTdiReport(TdispCommandResponseGetTdiReport {
                report_type: 3,
                report_buffer: vec![0, 0x81, 0xff, 0x42],
            }),
            "GetTdiReport",
        ),
        (
            Response::ModifyMmioRange(TdispCommandResponseModifyMmioRange {}),
            "ModifyMmioRange",
        ),
    ]
}

fn extraction<Payload: GuestToHostResponseVariant + PartialEq + std::fmt::Debug>(
    name: &str,
    expected: Payload,
) {
    for (variant, actual_name) in variants() {
        let response = GuestToHostResponse {
            result: 1,
            tdi_state_before: 1,
            tdi_state_after: 3,
            response: Some(variant),
        };
        assert_eq!(response.type_name(), Some(actual_name));
        let decoded = GuestToHostResponse::decode(response.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded, response);
        let result = decoded.response::<Payload>();
        if actual_name == name {
            assert_eq!(result.unwrap(), expected);
        } else {
            assert!(matches!(result, Err(TdispGuestOperationError::Unknown)));
        }
    }
    assert!(matches!(
        GuestToHostResponse {
            result: 1,
            ..Default::default()
        }
        .response::<Payload>(),
        Err(TdispGuestOperationError::Unknown)
    ));
}

// Test: Extract every typed payload from all six oneof responses after a
//       protobuf round trip, and reject absent payloads.
// Expected: Only the matching successful variant yields its unchanged payload.
//       Mismatches and missing payloads return Unknown.
#[test]
fn typed_extraction_requires_matching_successful_response() {
    for (variant, name) in variants() {
        match variant {
            Response::GetDeviceInterfaceInfo(payload) => extraction(name, payload),
            Response::Bind(payload) => extraction(name, payload),
            Response::StartTdi(payload) => extraction(name, payload),
            Response::Unbind(payload) => extraction(name, payload),
            Response::GetTdiReport(payload) => extraction(name, payload),
            Response::ModifyMmioRange(payload) => extraction(name, payload),
        }
    }
    assert_eq!(GuestToHostResponse::default().type_name(), None);
}

// Test: Round-trip each command kind with a distinctive device ID and nonzero
//       request parameters, and check an absent command.
// Expected: Type names identify the active command and transport preserves all
//       fields. A missing command has no type name.
#[test]
fn command_names_and_payloads_survive_protobuf_transport() {
    let commands = [
        (
            Command::GetDeviceInterfaceInfo(TdispCommandRequestGetDeviceInterfaceInfo {
                guest_protocol_type: 2,
            }),
            "GetDeviceInterfaceInfo",
        ),
        (Command::Bind(TdispCommandRequestBind {}), "Bind"),
        (
            Command::StartTdi(TdispCommandRequestStartTdi {}),
            "StartTdi",
        ),
        (
            Command::Unbind(TdispCommandRequestUnbind { unbind_reason: 4 }),
            "Unbind",
        ),
        (
            Command::GetTdiReport(TdispCommandRequestGetTdiReport { report_type: 3 }),
            "GetTdiReport",
        ),
        (
            Command::ModifyMmioRange(TdispCommandRequestModifyMmioRange {
                action: 1,
                range_id: 5,
                gpa_base: 0x8877665544332000,
                range_len_bytes: 0x12345000,
            }),
            "ModifyMmioRange",
        ),
    ];
    for (command, name) in commands {
        let command = GuestToHostCommand {
            device_id: 0x8877665544332211,
            command: Some(command),
        };
        assert_eq!(command.type_name(), Some(name));
        assert_eq!(
            GuestToHostCommand::decode(command.encode_to_vec().as_slice()).unwrap(),
            command
        );
    }
    assert_eq!(GuestToHostCommand::default().type_name(), None);
}

// Test: Decode before/after state pairs independently for every valid state,
//       Uninitialized, and unknown integers.
// Expected: Only Unlocked, Locked, and Run are host-advertisable. Each accessor
//       reads its own field; unknown and uninitialized states return None.
#[test]
fn advertised_states_reject_uninitialized_and_unknown_values() {
    for before in [-1, 0, 1, 2, 3, 4, i32::MAX] {
        for after in [-1, 0, 1, 2, 3, 4, i32::MAX] {
            let response = GuestToHostResponse {
                tdi_state_before: before,
                tdi_state_after: after,
                ..Default::default()
            };
            let expected = |value| match value {
                1 => Some(TdispTdiState::Unlocked),
                2 => Some(TdispTdiState::Locked),
                3 => Some(TdispTdiState::Run),
                _ => None,
            };
            assert_eq!(response.tdi_state_before_enum(), expected(before));
            assert_eq!(response.tdi_state_after_enum(), expected(after));
        }
    }
}

// Test: Convert every protocol result in both directions and attempt extraction
//       on each failure with an otherwise valid bind payload.
// Expected: Named error identities are exact and failures override payloads.
//       Success is not an error; unknown wire results become Unknown.
#[test]
fn error_identity_and_failure_precedence_are_preserved() {
    macro_rules! error {
        ($name:ident) => {{
            let code = TdispGuestOperationErrorCode::$name;
            assert!(matches!(
                TdispGuestOperationError::from(code),
                TdispGuestOperationError::$name
            ));
            assert_eq!(
                TdispGuestOperationErrorCode::from(TdispGuestOperationError::$name),
                code
            );
            let response = GuestToHostResponse {
                result: code as i32,
                response: Some(Response::Bind(TdispCommandResponseBind {})),
                ..Default::default()
            };
            assert_eq!(response.error_code(), Some(code));
            assert!(matches!(
                response.response::<TdispCommandResponseBind>(),
                Err(TdispGuestOperationError::$name)
            ));
        }};
    }
    error!(Unknown);
    error!(InvalidGuestProtocolRequest);
    error!(InvalidDeviceState);
    error!(InvalidGuestUnbindReason);
    error!(InvalidGuestCommandId);
    error!(NotImplemented);
    error!(HostFailedToProcessCommand);
    error!(InvalidGuestAttestationReportState);
    error!(InvalidGuestAttestationReportType);
    assert!(matches!(
        TdispGuestOperationError::from(TdispGuestOperationErrorCode::Success),
        TdispGuestOperationError::Success
    ));
    assert_eq!(
        TdispGuestOperationErrorCode::from(TdispGuestOperationError::Success),
        TdispGuestOperationErrorCode::Success
    );
    // Result codes are currently 0..=9; 10 is the next undefined value.
    for result in [-1, 10, i32::MAX] {
        let response = GuestToHostResponse {
            result,
            response: Some(Response::Bind(TdispCommandResponseBind {})),
            ..Default::default()
        };
        assert_eq!(response.error_code(), None);
        assert!(matches!(
            response.response::<TdispCommandResponseBind>(),
            Err(TdispGuestOperationError::Unknown)
        ));
    }
}

// Test: Decode literal host packets with an unknown response variant, an
//       unrelated future field, and an unknown result with a known payload.
// Expected: Future fields preserve a recognized report. Success without a known
//       variant and unknown status never yield a successful typed response.
#[test]
fn future_wire_fields_do_not_turn_unknown_responses_into_success() {
    // Success, Locked -> Run, and unknown length-delimited field 10 (0x52).
    // No recognized response payload is present.
    let future = [0x08, 1, 0x10, 2, 0x18, 3, 0x52, 4, 0x81, 0, 0xff, 0x27];
    let response = GuestToHostResponse::decode(future.as_slice()).unwrap();
    assert_eq!(response.error_code(), Some(TdispGuestOperationErrorCode::Success));
    assert_eq!(response.tdi_state_before_enum(), Some(TdispTdiState::Locked));
    assert_eq!(response.tdi_state_after_enum(), Some(TdispTdiState::Run));
    assert_eq!(response.type_name(), None);
    assert!(matches!(
        response.response::<TdispCommandResponseStartTdi>(),
        Err(TdispGuestOperationError::Unknown)
    ));

    // Success and GetTdiReport in field 6 (0x32): report type 3 and four bytes.
    let known = [0x08, 1, 0x32, 8, 0x08, 3, 0x12, 4, 0x81, 0, 0xff, 0x27];
    let mut extended = known.to_vec();
    // Append unrelated length-delimited field 15 (0x7a) with four bytes.
    extended.extend_from_slice(&[0x7a, 4, 0x81, 0, 0xff, 0x27]);
    for wire in [known.as_slice(), extended.as_slice()] {
        let response = GuestToHostResponse::decode(wire).unwrap();
        let report = response.response::<TdispCommandResponseGetTdiReport>().unwrap();
        assert_eq!(report.report_type, 3);
        assert_eq!(report.report_buffer, [0x81, 0, 0xff, 0x27]);
    }

    // Unknown status 127 (0x7f) and an empty Bind payload in field 5 (0x2a).
    let response = GuestToHostResponse::decode([0x08, 0x7f, 0x2a, 0].as_slice()).unwrap();
    assert_eq!(response.result, 0x7f);
    assert_eq!(response.error_code(), None);
    assert_eq!(response.type_name(), Some("Bind"));
    assert!(matches!(
        response.response::<TdispCommandResponseBind>(),
        Err(TdispGuestOperationError::Unknown)
    ));
}
