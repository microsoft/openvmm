// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! End-to-end tests that drive the TDISP state machine through the full
//! guest-to-host command protocol: commands are built, serialized to bytes,
//! deserialized back, dispatched to a [`TdispHostDeviceTargetEmulator`], and
//! the resulting [`GuestToHostResponse`] is serialized and deserialized in turn.
//! All responses -- including error responses -- exercise the full wire round-trip.

use crate::TdispHostDeviceTarget;
use crate::TdispHostDeviceTargetEmulator;
use crate::serialize_proto::deserialize_command;
use crate::serialize_proto::deserialize_response;
use crate::serialize_proto::serialize_command;
use crate::serialize_proto::serialize_response;
use crate::test_helpers::TDISP_MOCK_GUEST_PROTOCOL;
use crate::tests::mocks::LastCall;
use crate::tests::mocks::new_emulator;
use tdisp_proto::GuestToHostCommand;
use tdisp_proto::TdispCommandRequestBind;
use tdisp_proto::TdispCommandRequestGetDeviceInterfaceInfo;
use tdisp_proto::TdispCommandRequestGetTdiReport;
use tdisp_proto::TdispCommandRequestModifyMmioRange;
use tdisp_proto::TdispCommandRequestStartTdi;
use tdisp_proto::TdispCommandRequestUnbind;
use tdisp_proto::TdispGuestOperationErrorCode;
use tdisp_proto::TdispGuestProtocolType;
use tdisp_proto::TdispGuestUnbindReason;
use tdisp_proto::TdispMmioRangeAction;
use tdisp_proto::TdispReportType;
use tdisp_proto::TdispTdiState;
use tdisp_proto::guest_to_host_command::Command;
use tdisp_proto::guest_to_host_response::Response;

// ── Dispatch helpers ──────────────────────────────────────────────────────────

/// Serialize `cmd` to bytes, deserialize it, pass it to the emulator, and
/// return the raw `GuestToHostResponse`.
fn dispatch(
    emulator: &mut TdispHostDeviceTargetEmulator,
    cmd: GuestToHostCommand,
) -> tdisp_proto::GuestToHostResponse {
    let bytes = serialize_command(&cmd);
    let cmd = deserialize_command(&bytes).unwrap();
    emulator.tdisp_handle_guest_command(cmd).unwrap()
}

/// Like [`dispatch`], but also round-trips the response through
/// `serialize_response` + `deserialize_response`.
fn dispatch_roundtrip(
    emulator: &mut TdispHostDeviceTargetEmulator,
    cmd: GuestToHostCommand,
) -> tdisp_proto::GuestToHostResponse {
    let resp = dispatch(emulator, cmd);
    let bytes = serialize_response(&resp);
    deserialize_response(&bytes).unwrap()
}

// ── Command builders ──────────────────────────────────────────────────────────

fn negotiate_cmd(device_id: u64) -> GuestToHostCommand {
    GuestToHostCommand {
        device_id,
        command: Some(Command::GetDeviceInterfaceInfo(
            TdispCommandRequestGetDeviceInterfaceInfo {
                guest_protocol_type: TDISP_MOCK_GUEST_PROTOCOL as i32,
            },
        )),
    }
}

fn bind_cmd(device_id: u64) -> GuestToHostCommand {
    GuestToHostCommand {
        device_id,
        command: Some(Command::Bind(TdispCommandRequestBind {})),
    }
}

fn start_tdi_cmd(device_id: u64) -> GuestToHostCommand {
    GuestToHostCommand {
        device_id,
        command: Some(Command::StartTdi(TdispCommandRequestStartTdi {})),
    }
}

fn unbind_cmd(device_id: u64, reason: TdispGuestUnbindReason) -> GuestToHostCommand {
    GuestToHostCommand {
        device_id,
        command: Some(Command::Unbind(TdispCommandRequestUnbind {
            unbind_reason: reason as i32,
        })),
    }
}

fn get_device_interface_info_cmd(device_id: u64) -> GuestToHostCommand {
    GuestToHostCommand {
        device_id,
        command: Some(Command::GetDeviceInterfaceInfo(
            TdispCommandRequestGetDeviceInterfaceInfo {
                guest_protocol_type: TDISP_MOCK_GUEST_PROTOCOL as i32,
            },
        )),
    }
}

fn get_tdi_report_cmd(device_id: u64, report_type: TdispReportType) -> GuestToHostCommand {
    GuestToHostCommand {
        device_id,
        command: Some(Command::GetTdiReport(TdispCommandRequestGetTdiReport {
            report_type: report_type as i32,
        })),
    }
}

fn modify_mmio_range_cmd(
    device_id: u64,
    action: TdispMmioRangeAction,
    range_id: u32,
    gpa_base: u64,
    range_len_bytes: u64,
) -> GuestToHostCommand {
    GuestToHostCommand {
        device_id,
        command: Some(Command::ModifyMmioRange(
            TdispCommandRequestModifyMmioRange {
                action: action as i32,
                range_id,
                gpa_base,
                range_len_bytes,
            },
        )),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

// ── Protocol negotiation ──────────────────────────────────────────────────────

/// A valid protocol type is accepted, the host interface is consulted, and the
/// negotiated protocol info is returned in the response.
#[test]
fn test_negotiate_protocol_succeeds_with_valid_protocol() {
    let mut mock = new_emulator();

    let resp = dispatch_roundtrip(&mut mock.emulator, negotiate_cmd(1));
    assert_eq!(resp.result, TdispGuestOperationErrorCode::Success as i32);
    assert_eq!(resp.tdi_state_before, TdispTdiState::Unlocked as i32);
    assert_eq!(resp.tdi_state_after, TdispTdiState::Unlocked as i32);

    let Some(Response::GetDeviceInterfaceInfo(r)) = resp.response else {
        panic!("expected GetDeviceInterfaceInfo response");
    };
    let info = r.interface_info.unwrap();
    assert_eq!(info.guest_protocol_type, TDISP_MOCK_GUEST_PROTOCOL as i32);
    assert_eq!(*mock.last_call.lock(), Some(LastCall::NegotiateProtocol));
}

/// An unrecognized protocol type integer is rejected before the host interface
/// is consulted, so the state remains Unlocked and no host call is recorded.
#[test]
fn test_negotiate_protocol_fails_with_invalid_protocol() {
    let mut mock = new_emulator();

    let cmd = GuestToHostCommand {
        device_id: 1,
        command: Some(Command::GetDeviceInterfaceInfo(
            TdispCommandRequestGetDeviceInterfaceInfo {
                guest_protocol_type: TdispGuestProtocolType::Invalid as i32, // not a valid TdispGuestProtocolType
            },
        )),
    };
    let resp = dispatch_roundtrip(&mut mock.emulator, cmd);
    assert_eq!(
        resp.result,
        TdispGuestOperationErrorCode::InvalidGuestProtocolRequest as i32
    );
    assert_eq!(resp.tdi_state_before, TdispTdiState::Unlocked as i32);
    assert_eq!(resp.tdi_state_after, TdispTdiState::Unlocked as i32);
    // Host interface is not consulted for an unrecognized protocol value.
    assert_eq!(*mock.last_call.lock(), None);
}

// ── Full lifecycle ────────────────────────────────────────────────────────────

/// Full lifecycle via serialized commands: Unlocked -> Locked -> Run -> Unlocked.
/// Each step is verified against expected state transitions, result codes, response
/// variants, and host-interface calls.
#[test]
fn test_full_lifecycle_via_serialized_commands() {
    let mut mock = new_emulator();
    const DEVICE_ID: u64 = 42;

    // Negotiate protocol before any state transitions.
    dispatch_roundtrip(&mut mock.emulator, negotiate_cmd(DEVICE_ID));

    // Bind: Unlocked -> Locked
    let resp = dispatch_roundtrip(&mut mock.emulator, bind_cmd(DEVICE_ID));
    assert_eq!(resp.result, TdispGuestOperationErrorCode::Success as i32);
    assert_eq!(resp.tdi_state_before, TdispTdiState::Unlocked as i32);
    assert_eq!(resp.tdi_state_after, TdispTdiState::Locked as i32);
    assert!(matches!(resp.response, Some(Response::Bind(_))));
    assert_eq!(*mock.last_call.lock(), Some(LastCall::BindDevice));

    // StartTdi: Locked -> Run
    let resp = dispatch_roundtrip(&mut mock.emulator, start_tdi_cmd(DEVICE_ID));
    assert_eq!(resp.result, TdispGuestOperationErrorCode::Success as i32);
    assert_eq!(resp.tdi_state_before, TdispTdiState::Locked as i32);
    assert_eq!(resp.tdi_state_after, TdispTdiState::Run as i32);
    assert!(matches!(resp.response, Some(Response::StartTdi(_))));
    assert_eq!(*mock.last_call.lock(), Some(LastCall::StartDevice));

    // Unbind: Run -> Unlocked
    let resp = dispatch_roundtrip(
        &mut mock.emulator,
        unbind_cmd(DEVICE_ID, TdispGuestUnbindReason::Graceful),
    );
    assert_eq!(resp.result, TdispGuestOperationErrorCode::Success as i32);
    assert_eq!(resp.tdi_state_before, TdispTdiState::Run as i32);
    assert_eq!(resp.tdi_state_after, TdispTdiState::Unlocked as i32);
    assert!(matches!(resp.response, Some(Response::Unbind(_))));
    assert_eq!(*mock.last_call.lock(), Some(LastCall::UnbindDevice));
}

/// GetDeviceInterfaceInfo negotiates the protocol with the host interface and
/// returns the device capabilities. It does not change state.
#[test]
fn test_get_device_interface_info_command() {
    let mut mock = new_emulator();

    let resp = dispatch_roundtrip(&mut mock.emulator, get_device_interface_info_cmd(1));
    assert_eq!(resp.result, TdispGuestOperationErrorCode::Success as i32);
    assert_eq!(resp.tdi_state_before, TdispTdiState::Unlocked as i32);
    assert_eq!(resp.tdi_state_after, TdispTdiState::Unlocked as i32);

    let Some(Response::GetDeviceInterfaceInfo(r)) = resp.response else {
        panic!("expected GetDeviceInterfaceInfo response");
    };
    let info = r.interface_info.unwrap();
    assert_eq!(info.guest_protocol_type, TDISP_MOCK_GUEST_PROTOCOL as i32);

    // GetDeviceInterfaceInfo delegates to the host interface for negotiation.
    assert_eq!(*mock.last_call.lock(), Some(LastCall::NegotiateProtocol));
}

/// GetTdiReport succeeds in the Locked state and returns the report from the
/// host without changing state.
#[test]
fn test_get_tdi_report_in_locked_state() {
    let mut mock = new_emulator();
    dispatch_roundtrip(&mut mock.emulator, negotiate_cmd(1));
    dispatch_roundtrip(&mut mock.emulator, bind_cmd(1)); // Unlocked -> Locked

    let mock_interface_report: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x01, 0x02, 0x03];
    *mock.report_buffer.lock() = mock_interface_report.clone();

    let resp = dispatch_roundtrip(
        &mut mock.emulator,
        get_tdi_report_cmd(1, TdispReportType::InterfaceReport),
    );
    assert_eq!(resp.result, TdispGuestOperationErrorCode::Success as i32);
    assert_eq!(resp.tdi_state_before, TdispTdiState::Locked as i32);
    assert_eq!(resp.tdi_state_after, TdispTdiState::Locked as i32);

    let Some(Response::GetTdiReport(r)) = resp.response else {
        panic!("expected GetTdiReport response");
    };
    assert_eq!(r.report_type, TdispReportType::InterfaceReport as i32);
    assert!(!r.report_buffer.is_empty());
    assert_eq!(r.report_buffer, mock_interface_report);
    assert_eq!(
        *mock.last_call.lock(),
        Some(LastCall::GetDeviceReport(TdispReportType::InterfaceReport))
    );
}

/// GetTdiReport also succeeds in the Run state without changing state.
#[test]
fn test_get_tdi_report_in_run_state() {
    let mut mock = new_emulator();
    dispatch_roundtrip(&mut mock.emulator, negotiate_cmd(1));
    dispatch_roundtrip(&mut mock.emulator, bind_cmd(1)); // Unlocked -> Locked
    dispatch_roundtrip(&mut mock.emulator, start_tdi_cmd(1)); // Locked -> Run

    let mock_interface_report: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x01, 0x02, 0x03];
    *mock.report_buffer.lock() = mock_interface_report.clone();

    let resp = dispatch_roundtrip(
        &mut mock.emulator,
        get_tdi_report_cmd(1, TdispReportType::InterfaceReport),
    );
    assert_eq!(resp.result, TdispGuestOperationErrorCode::Success as i32);
    assert_eq!(resp.tdi_state_before, TdispTdiState::Run as i32);
    assert_eq!(resp.tdi_state_after, TdispTdiState::Run as i32);

    let Some(Response::GetTdiReport(r)) = resp.response else {
        panic!("expected GetTdiReport response");
    };
    assert_eq!(r.report_type, TdispReportType::InterfaceReport as i32);
    assert!(!r.report_buffer.is_empty());
    assert_eq!(r.report_buffer, mock_interface_report);
    assert_eq!(
        *mock.last_call.lock(),
        Some(LastCall::GetDeviceReport(TdispReportType::InterfaceReport))
    )
}

/// Sending a Bind command while already Locked returns an error and the
/// internal unbind_all resets the device to Unlocked. A subsequent Bind
/// then succeeds.
#[test]
fn test_bind_from_locked_returns_error_and_resets_to_unlocked() {
    let mut mock = new_emulator();
    dispatch_roundtrip(&mut mock.emulator, negotiate_cmd(1));
    dispatch_roundtrip(&mut mock.emulator, bind_cmd(1)); // Unlocked -> Locked

    // Second bind from Locked: error path.
    let resp = dispatch_roundtrip(&mut mock.emulator, bind_cmd(1));
    assert_eq!(
        resp.result,
        TdispGuestOperationErrorCode::InvalidDeviceState as i32
    );
    assert_eq!(resp.tdi_state_before, TdispTdiState::Locked as i32);
    assert_eq!(resp.tdi_state_after, TdispTdiState::Unlocked as i32);
    // The failed bind triggers an internal unbind_all.
    assert_eq!(*mock.last_call.lock(), Some(LastCall::UnbindDevice));

    // After the automatic reset the device is back in Unlocked and can be bound again.
    let resp = dispatch_roundtrip(&mut mock.emulator, bind_cmd(1));
    assert_eq!(resp.result, TdispGuestOperationErrorCode::Success as i32);
    assert_eq!(resp.tdi_state_after, TdispTdiState::Locked as i32);
    assert_eq!(*mock.last_call.lock(), Some(LastCall::BindDevice));
}

/// StartTdi from the Unlocked state returns an error and resets to Unlocked.
#[test]
fn test_start_tdi_from_unlocked_returns_error() {
    let mut mock = new_emulator();
    dispatch_roundtrip(&mut mock.emulator, negotiate_cmd(1));

    let resp = dispatch_roundtrip(&mut mock.emulator, start_tdi_cmd(1));
    assert_eq!(
        resp.result,
        TdispGuestOperationErrorCode::InvalidDeviceState as i32
    );
    assert_eq!(resp.tdi_state_before, TdispTdiState::Unlocked as i32);
    assert_eq!(resp.tdi_state_after, TdispTdiState::Unlocked as i32);
    assert_eq!(*mock.last_call.lock(), Some(LastCall::UnbindDevice));
}

/// Unbind from the Unlocked state is explicitly permitted and leaves the
/// device in Unlocked.
#[test]
fn test_unbind_from_unlocked_is_allowed() {
    let mut mock = new_emulator();
    dispatch_roundtrip(&mut mock.emulator, negotiate_cmd(1));

    let resp = dispatch_roundtrip(
        &mut mock.emulator,
        unbind_cmd(1, TdispGuestUnbindReason::Graceful),
    );
    assert_eq!(resp.result, TdispGuestOperationErrorCode::Success as i32);
    assert_eq!(resp.tdi_state_before, TdispTdiState::Unlocked as i32);
    assert_eq!(resp.tdi_state_after, TdispTdiState::Unlocked as i32);
    assert!(matches!(resp.response, Some(Response::Unbind(_))));
    assert_eq!(*mock.last_call.lock(), Some(LastCall::UnbindDevice));
}

/// After a full Unlocked -> Locked -> Run -> Unlocked cycle the device can
/// be bound and started again from scratch.
#[test]
fn test_rebind_after_full_lifecycle() {
    let mut mock = new_emulator();
    const DEVICE_ID: u64 = 7;

    dispatch_roundtrip(&mut mock.emulator, negotiate_cmd(DEVICE_ID));

    // First cycle
    dispatch_roundtrip(&mut mock.emulator, bind_cmd(DEVICE_ID));
    dispatch_roundtrip(&mut mock.emulator, start_tdi_cmd(DEVICE_ID));
    dispatch_roundtrip(
        &mut mock.emulator,
        unbind_cmd(DEVICE_ID, TdispGuestUnbindReason::Graceful),
    );

    // Second cycle: device must behave identically
    let resp = dispatch_roundtrip(&mut mock.emulator, bind_cmd(DEVICE_ID));
    assert_eq!(resp.result, TdispGuestOperationErrorCode::Success as i32);
    assert_eq!(resp.tdi_state_before, TdispTdiState::Unlocked as i32);
    assert_eq!(resp.tdi_state_after, TdispTdiState::Locked as i32);
    assert_eq!(*mock.last_call.lock(), Some(LastCall::BindDevice));

    let resp = dispatch_roundtrip(&mut mock.emulator, start_tdi_cmd(DEVICE_ID));
    assert_eq!(resp.result, TdispGuestOperationErrorCode::Success as i32);
    assert_eq!(resp.tdi_state_after, TdispTdiState::Run as i32);
    assert_eq!(*mock.last_call.lock(), Some(LastCall::StartDevice));
}

// ── ModifyMmioRange ────────────────────────────────────────────────────

/// The command is accepted in Locked, forwards the guest's values to the host
/// interface unchanged, and does not transition the TDI.
#[test]
fn test_modify_mmio_range_succeeds_in_locked() {
    let mut mock = new_emulator();
    const DEVICE_ID: u64 = 3;

    dispatch_roundtrip(&mut mock.emulator, negotiate_cmd(DEVICE_ID));
    dispatch_roundtrip(&mut mock.emulator, bind_cmd(DEVICE_ID));

    let resp = dispatch_roundtrip(
        &mut mock.emulator,
        modify_mmio_range_cmd(
            DEVICE_ID,
            TdispMmioRangeAction::UnblockMmioRange,
            2,
            0xe000_0000,
            0x10_0000,
        ),
    );
    assert_eq!(resp.result, TdispGuestOperationErrorCode::Success as i32);
    assert_eq!(resp.tdi_state_before, TdispTdiState::Locked as i32);
    assert_eq!(resp.tdi_state_after, TdispTdiState::Locked as i32);
    assert!(matches!(resp.response, Some(Response::ModifyMmioRange(_))));
    assert_eq!(
        *mock.last_call.lock(),
        Some(LastCall::ModifyMmioRange {
            action: TdispMmioRangeAction::UnblockMmioRange,
            range_id: 2,
            gpa_base: 0xe000_0000,
            range_len_bytes: 0x10_0000,
        })
    );
}

/// The command is equally valid in Run, and again leaves the state alone.
#[test]
fn test_modify_mmio_range_succeeds_in_run() {
    let mut mock = new_emulator();
    const DEVICE_ID: u64 = 3;

    dispatch_roundtrip(&mut mock.emulator, negotiate_cmd(DEVICE_ID));
    dispatch_roundtrip(&mut mock.emulator, bind_cmd(DEVICE_ID));
    dispatch_roundtrip(&mut mock.emulator, start_tdi_cmd(DEVICE_ID));

    let resp = dispatch_roundtrip(
        &mut mock.emulator,
        modify_mmio_range_cmd(
            DEVICE_ID,
            TdispMmioRangeAction::BlockMmioRange,
            0,
            0xf000_0000,
            0x2_0000_0000,
        ),
    );
    assert_eq!(resp.result, TdispGuestOperationErrorCode::Success as i32);
    assert_eq!(resp.tdi_state_before, TdispTdiState::Run as i32);
    assert_eq!(resp.tdi_state_after, TdispTdiState::Run as i32);
    assert_eq!(
        *mock.last_call.lock(),
        Some(LastCall::ModifyMmioRange {
            action: TdispMmioRangeAction::BlockMmioRange,
            range_id: 0,
            gpa_base: 0xf000_0000,
            // Larger than u32::MAX, confirming the 64-bit length survives the
            // whole path.
            range_len_bytes: 0x2_0000_0000,
        })
    );
}

/// In Unlocked the command is rejected with InvalidDeviceState. Unlike
/// StartTdi and GetTdiReport, this must NOT tear the TDI down: the state is
/// still Unlocked afterwards and the host interface was never called.
#[test]
fn test_modify_mmio_range_fails_in_unlocked_without_teardown() {
    let mut mock = new_emulator();
    const DEVICE_ID: u64 = 3;

    dispatch_roundtrip(&mut mock.emulator, negotiate_cmd(DEVICE_ID));
    assert_eq!(*mock.last_call.lock(), Some(LastCall::NegotiateProtocol));

    let resp = dispatch_roundtrip(
        &mut mock.emulator,
        modify_mmio_range_cmd(
            DEVICE_ID,
            TdispMmioRangeAction::UnblockMmioRange,
            1,
            0xd000_0000,
            0x1000,
        ),
    );
    assert_eq!(
        resp.result,
        TdispGuestOperationErrorCode::InvalidDeviceState as i32
    );
    assert_eq!(resp.tdi_state_before, TdispTdiState::Unlocked as i32);
    assert_eq!(resp.tdi_state_after, TdispTdiState::Unlocked as i32);

    // No unbind was issued and the host never saw the range.
    assert_eq!(*mock.last_call.lock(), Some(LastCall::NegotiateProtocol));

    // The TDI is still usable: a bind right after the rejected modify works.
    let resp = dispatch_roundtrip(&mut mock.emulator, bind_cmd(DEVICE_ID));
    assert_eq!(resp.result, TdispGuestOperationErrorCode::Success as i32);
    assert_eq!(resp.tdi_state_after, TdispTdiState::Locked as i32);
}

/// A range_id that does not fit in a u16 is rejected before the state machine
/// is consulted, since the wire type is wider than the BAR index it carries.
#[test]
fn test_modify_mmio_range_rejects_oversized_range_id() {
    let mut mock = new_emulator();
    const DEVICE_ID: u64 = 3;

    dispatch_roundtrip(&mut mock.emulator, negotiate_cmd(DEVICE_ID));
    dispatch_roundtrip(&mut mock.emulator, bind_cmd(DEVICE_ID));

    let resp = dispatch_roundtrip(
        &mut mock.emulator,
        modify_mmio_range_cmd(
            DEVICE_ID,
            TdispMmioRangeAction::UnblockMmioRange,
            u32::MAX,
            0xd000_0000,
            0x1000,
        ),
    );
    assert_eq!(
        resp.result,
        TdispGuestOperationErrorCode::InvalidGuestCommandId as i32
    );
    assert_eq!(resp.tdi_state_after, TdispTdiState::Locked as i32);
    assert_eq!(*mock.last_call.lock(), Some(LastCall::BindDevice));
}

/// The Invalid action is rejected in a valid state, so the guest cannot use a
/// zero-valued action to reach the host interface.
#[test]
fn test_modify_mmio_range_rejects_invalid_action() {
    let mut mock = new_emulator();
    const DEVICE_ID: u64 = 3;

    dispatch_roundtrip(&mut mock.emulator, negotiate_cmd(DEVICE_ID));
    dispatch_roundtrip(&mut mock.emulator, bind_cmd(DEVICE_ID));

    let resp = dispatch_roundtrip(
        &mut mock.emulator,
        modify_mmio_range_cmd(
            DEVICE_ID,
            TdispMmioRangeAction::Invalid,
            1,
            0xd000_0000,
            0x1000,
        ),
    );
    assert_eq!(
        resp.result,
        TdispGuestOperationErrorCode::InvalidGuestCommandId as i32
    );
    assert_eq!(resp.tdi_state_after, TdispTdiState::Locked as i32);
    assert_eq!(*mock.last_call.lock(), Some(LastCall::BindDevice));
}
