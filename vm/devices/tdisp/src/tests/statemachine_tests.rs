// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::TdispGuestOperationError;
use crate::TdispGuestRequestInterface;
use crate::tests::mocks::LastCall;
use crate::tests::mocks::new_machine;
use tdisp_proto::TdispGuestUnbindReason;
use tdisp_proto::TdispReportType;
use tdisp_proto::TdispTdiState;

// ── Valid forward-progress transitions ───────────────────────────────────────

#[test]
fn test_bind_transitions_unlocked_to_locked() {
    let mut mock = new_machine();
    mock.machine.request_lock_device_resources().unwrap();
    assert_eq!(mock.machine.state(), TdispTdiState::Locked);
    assert_eq!(*mock.last_call.lock(), Some(LastCall::BindDevice));
}

#[test]
fn test_start_tdi_transitions_locked_to_run() {
    let mut mock = new_machine();
    mock.machine.request_lock_device_resources().unwrap();
    mock.machine.request_start_tdi().unwrap();
    assert_eq!(mock.machine.state(), TdispTdiState::Run);
    assert_eq!(*mock.last_call.lock(), Some(LastCall::StartDevice));
}

#[test]
fn test_full_lifecycle() {
    let mut mock = new_machine();
    assert_eq!(mock.machine.state(), TdispTdiState::Unlocked);

    mock.machine.request_lock_device_resources().unwrap();
    assert_eq!(mock.machine.state(), TdispTdiState::Locked);
    assert_eq!(*mock.last_call.lock(), Some(LastCall::BindDevice));

    mock.machine.request_start_tdi().unwrap();
    assert_eq!(mock.machine.state(), TdispTdiState::Run);
    assert_eq!(*mock.last_call.lock(), Some(LastCall::StartDevice));

    mock.machine
        .request_unbind(TdispGuestUnbindReason::Graceful)
        .unwrap();
    assert_eq!(mock.machine.state(), TdispTdiState::Unlocked);
    assert_eq!(*mock.last_call.lock(), Some(LastCall::UnbindDevice));
}

// ── Unbind from each state ────────────────────────────────────────────────────

#[test]
fn test_unbind_from_locked_resets_to_unlocked() {
    let mut mock = new_machine();
    mock.machine.request_lock_device_resources().unwrap();
    mock.machine
        .request_unbind(TdispGuestUnbindReason::Graceful)
        .unwrap();
    assert_eq!(mock.machine.state(), TdispTdiState::Unlocked);
    assert_eq!(*mock.last_call.lock(), Some(LastCall::UnbindDevice));
}

#[test]
fn test_unbind_from_run_resets_to_unlocked() {
    let mut mock = new_machine();
    mock.machine.request_lock_device_resources().unwrap();
    mock.machine.request_start_tdi().unwrap();
    mock.machine
        .request_unbind(TdispGuestUnbindReason::Graceful)
        .unwrap();
    assert_eq!(mock.machine.state(), TdispTdiState::Unlocked);
    assert_eq!(*mock.last_call.lock(), Some(LastCall::UnbindDevice));
}

#[test]
fn test_unbind_from_unlocked_stays_unlocked() {
    let mut mock = new_machine();
    // Unlocked -> Unlocked is an explicitly permitted transition.
    mock.machine
        .request_unbind(TdispGuestUnbindReason::Graceful)
        .unwrap();
    assert_eq!(mock.machine.state(), TdispTdiState::Unlocked);
    assert_eq!(*mock.last_call.lock(), Some(LastCall::UnbindDevice));
}

#[test]
fn test_rebind_after_unbind() {
    let mut mock = new_machine();
    mock.machine.request_lock_device_resources().unwrap();
    mock.machine
        .request_unbind(TdispGuestUnbindReason::Graceful)
        .unwrap();
    // The device is back in Unlocked and can go through the flow again.
    mock.machine.request_lock_device_resources().unwrap();
    assert_eq!(mock.machine.state(), TdispTdiState::Locked);
    assert_eq!(*mock.last_call.lock(), Some(LastCall::BindDevice));
}

#[test]
fn test_unbind_with_unknown_reason_still_succeeds() {
    let mut mock = new_machine();
    mock.machine.request_lock_device_resources().unwrap();
    // Unknown is not a valid guest-initiated reason, but the unbind still
    // completes (the reason is recorded as invalid in the history).
    mock.machine
        .request_unbind(TdispGuestUnbindReason::Unknown)
        .unwrap();
    assert_eq!(mock.machine.state(), TdispTdiState::Unlocked);
    assert_eq!(*mock.last_call.lock(), Some(LastCall::UnbindDevice));
}

// ── Invalid transitions (error + automatic reset to Unlocked) ────────────────

#[test]
fn test_bind_from_locked_fails_and_resets_to_unlocked() {
    let mut mock = new_machine();
    mock.machine.request_lock_device_resources().unwrap();

    let err = mock.machine.request_lock_device_resources().unwrap_err();
    assert!(matches!(err, TdispGuestOperationError::InvalidDeviceState));
    assert_eq!(mock.machine.state(), TdispTdiState::Unlocked);
    // The failed bind triggers an internal unbind_all to reset the device.
    assert_eq!(*mock.last_call.lock(), Some(LastCall::UnbindDevice));
}

#[test]
fn test_bind_from_run_fails_and_resets_to_unlocked() {
    let mut mock = new_machine();
    mock.machine.request_lock_device_resources().unwrap();
    mock.machine.request_start_tdi().unwrap();

    let err = mock.machine.request_lock_device_resources().unwrap_err();
    assert!(matches!(err, TdispGuestOperationError::InvalidDeviceState));
    assert_eq!(mock.machine.state(), TdispTdiState::Unlocked);
    assert_eq!(*mock.last_call.lock(), Some(LastCall::UnbindDevice));
}

#[test]
fn test_start_tdi_from_unlocked_fails_and_resets_to_unlocked() {
    let mut mock = new_machine();

    let err = mock.machine.request_start_tdi().unwrap_err();
    assert!(matches!(err, TdispGuestOperationError::InvalidDeviceState));
    assert_eq!(mock.machine.state(), TdispTdiState::Unlocked);
    assert_eq!(*mock.last_call.lock(), Some(LastCall::UnbindDevice));
}

#[test]
fn test_start_tdi_from_run_fails_and_resets_to_unlocked() {
    let mut mock = new_machine();
    mock.machine.request_lock_device_resources().unwrap();
    mock.machine.request_start_tdi().unwrap();

    let err = mock.machine.request_start_tdi().unwrap_err();
    assert!(matches!(err, TdispGuestOperationError::InvalidDeviceState));
    assert_eq!(mock.machine.state(), TdispTdiState::Unlocked);
    assert_eq!(*mock.last_call.lock(), Some(LastCall::UnbindDevice));
}

// ── Attestation report ────────────────────────────────────────────────────────

#[test]
fn test_attestation_report_from_locked_succeeds() {
    let mut mock = new_machine();
    mock.machine.request_lock_device_resources().unwrap();

    let report = mock
        .machine
        .request_attestation_report(TdispReportType::InterfaceReport)
        .unwrap();
    assert!(!report.is_empty());
    // State must be unchanged after a successful report retrieval.
    assert_eq!(mock.machine.state(), TdispTdiState::Locked);
    assert_eq!(
        *mock.last_call.lock(),
        Some(LastCall::GetDeviceReport(TdispReportType::InterfaceReport))
    );
}

#[test]
fn test_attestation_report_from_run_succeeds() {
    let mut mock = new_machine();
    mock.machine.request_lock_device_resources().unwrap();
    mock.machine.request_start_tdi().unwrap();

    let report = mock
        .machine
        .request_attestation_report(TdispReportType::InterfaceReport)
        .unwrap();
    assert!(!report.is_empty());
    assert_eq!(mock.machine.state(), TdispTdiState::Run);
    assert_eq!(
        *mock.last_call.lock(),
        Some(LastCall::GetDeviceReport(TdispReportType::InterfaceReport))
    );
}

#[test]
fn test_attestation_report_from_unlocked_fails_and_resets_to_unlocked() {
    let mut mock = new_machine();

    let err = mock
        .machine
        .request_attestation_report(TdispReportType::InterfaceReport)
        .unwrap_err();
    assert!(matches!(
        err,
        TdispGuestOperationError::InvalidGuestAttestationReportState
    ));
    assert_eq!(mock.machine.state(), TdispTdiState::Unlocked);
    // The state-check failure triggers unbind_all before returning the error.
    assert_eq!(*mock.last_call.lock(), Some(LastCall::UnbindDevice));
}

#[test]
fn test_attestation_report_invalid_type_from_locked_returns_error_without_state_change() {
    let mut mock = new_machine();
    mock.machine.request_lock_device_resources().unwrap();

    // TdispReportType::Invalid is rejected before the host interface is
    // consulted, so the last call must still be the BindDevice from setup.
    let err = mock
        .machine
        .request_attestation_report(TdispReportType::Invalid)
        .unwrap_err();
    assert!(matches!(
        err,
        TdispGuestOperationError::InvalidGuestAttestationReportType
    ));
    assert_eq!(mock.machine.state(), TdispTdiState::Locked);
    assert_eq!(*mock.last_call.lock(), Some(LastCall::BindDevice));
}
