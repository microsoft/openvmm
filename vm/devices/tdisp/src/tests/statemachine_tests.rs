// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::Arc;

use parking_lot::Mutex;
use tdisp_proto::{TdispGuestUnbindReason, TdispReportType, TdispTdiState};

use crate::{
    TdispGuestOperationError, TdispGuestRequestInterface, TdispHostDeviceInterface,
    TdispHostStateMachine,
};

// ── Mock host interface ───────────────────────────────────────────────────────

/// Records which host-interface method was called most recently.
#[derive(Debug, PartialEq, Clone)]
enum LastCall {
    BindDevice,
    StartDevice,
    UnbindDevice,
    GetDeviceReport(TdispReportType),
}

struct TrackingHostInterface {
    last_call: Arc<Mutex<Option<LastCall>>>,
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

    fn tdisp_get_device_report(&mut self, report_type: TdispReportType) -> anyhow::Result<Vec<u8>> {
        *self.last_call.lock() = Some(LastCall::GetDeviceReport(report_type));
        Ok(vec![0xDE, 0xAD, 0xBE, 0xEF])
    }
}

/// Returns a fresh state machine paired with a handle for inspecting which
/// host-interface method was called most recently.
fn new_machine() -> (TdispHostStateMachine, Arc<Mutex<Option<LastCall>>>) {
    let last_call: Arc<Mutex<Option<LastCall>>> = Arc::new(Mutex::new(None));
    let interface = TrackingHostInterface {
        last_call: last_call.clone(),
    };
    let machine = TdispHostStateMachine::new(Arc::new(Mutex::new(interface)));
    (machine, last_call)
}

// ── Initial state ─────────────────────────────────────────────────────────────

#[test]
fn test_initial_state_is_unlocked() {
    let (machine, last_call) = new_machine();
    assert_eq!(machine.state(), TdispTdiState::Unlocked);
    // No host-interface method should have been called during construction.
    assert_eq!(*last_call.lock(), None);
}

// ── Valid forward-progress transitions ───────────────────────────────────────

#[test]
fn test_bind_transitions_unlocked_to_locked() {
    let (mut machine, last_call) = new_machine();
    machine.request_lock_device_resources().unwrap();
    assert_eq!(machine.state(), TdispTdiState::Locked);
    assert_eq!(*last_call.lock(), Some(LastCall::BindDevice));
}

#[test]
fn test_start_tdi_transitions_locked_to_run() {
    let (mut machine, last_call) = new_machine();
    machine.request_lock_device_resources().unwrap();
    machine.request_start_tdi().unwrap();
    assert_eq!(machine.state(), TdispTdiState::Run);
    assert_eq!(*last_call.lock(), Some(LastCall::StartDevice));
}

#[test]
fn test_full_lifecycle() {
    let (mut machine, last_call) = new_machine();
    assert_eq!(machine.state(), TdispTdiState::Unlocked);

    machine.request_lock_device_resources().unwrap();
    assert_eq!(machine.state(), TdispTdiState::Locked);
    assert_eq!(*last_call.lock(), Some(LastCall::BindDevice));

    machine.request_start_tdi().unwrap();
    assert_eq!(machine.state(), TdispTdiState::Run);
    assert_eq!(*last_call.lock(), Some(LastCall::StartDevice));

    machine
        .request_unbind(TdispGuestUnbindReason::Graceful)
        .unwrap();
    assert_eq!(machine.state(), TdispTdiState::Unlocked);
    assert_eq!(*last_call.lock(), Some(LastCall::UnbindDevice));
}

// ── Unbind from each state ────────────────────────────────────────────────────

#[test]
fn test_unbind_from_locked_resets_to_unlocked() {
    let (mut machine, last_call) = new_machine();
    machine.request_lock_device_resources().unwrap();
    machine
        .request_unbind(TdispGuestUnbindReason::Graceful)
        .unwrap();
    assert_eq!(machine.state(), TdispTdiState::Unlocked);
    assert_eq!(*last_call.lock(), Some(LastCall::UnbindDevice));
}

#[test]
fn test_unbind_from_run_resets_to_unlocked() {
    let (mut machine, last_call) = new_machine();
    machine.request_lock_device_resources().unwrap();
    machine.request_start_tdi().unwrap();
    machine
        .request_unbind(TdispGuestUnbindReason::Graceful)
        .unwrap();
    assert_eq!(machine.state(), TdispTdiState::Unlocked);
    assert_eq!(*last_call.lock(), Some(LastCall::UnbindDevice));
}

#[test]
fn test_unbind_from_unlocked_stays_unlocked() {
    let (mut machine, last_call) = new_machine();
    // Unlocked -> Unlocked is an explicitly permitted transition.
    machine
        .request_unbind(TdispGuestUnbindReason::Graceful)
        .unwrap();
    assert_eq!(machine.state(), TdispTdiState::Unlocked);
    assert_eq!(*last_call.lock(), Some(LastCall::UnbindDevice));
}

#[test]
fn test_rebind_after_unbind() {
    let (mut machine, last_call) = new_machine();
    machine.request_lock_device_resources().unwrap();
    machine
        .request_unbind(TdispGuestUnbindReason::Graceful)
        .unwrap();
    // The device is back in Unlocked and can go through the flow again.
    machine.request_lock_device_resources().unwrap();
    assert_eq!(machine.state(), TdispTdiState::Locked);
    assert_eq!(*last_call.lock(), Some(LastCall::BindDevice));
}

#[test]
fn test_unbind_with_unknown_reason_still_succeeds() {
    let (mut machine, last_call) = new_machine();
    machine.request_lock_device_resources().unwrap();
    // Unknown is not a valid guest-initiated reason, but the unbind still
    // completes (the reason is recorded as invalid in the history).
    machine
        .request_unbind(TdispGuestUnbindReason::Unknown)
        .unwrap();
    assert_eq!(machine.state(), TdispTdiState::Unlocked);
    assert_eq!(*last_call.lock(), Some(LastCall::UnbindDevice));
}

// ── Invalid transitions (error + automatic reset to Unlocked) ────────────────

#[test]
fn test_bind_from_locked_fails_and_resets_to_unlocked() {
    let (mut machine, last_call) = new_machine();
    machine.request_lock_device_resources().unwrap();

    let err = machine.request_lock_device_resources().unwrap_err();
    assert!(matches!(err, TdispGuestOperationError::InvalidDeviceState));
    assert_eq!(machine.state(), TdispTdiState::Unlocked);
    // The failed bind triggers an internal unbind_all to reset the device.
    assert_eq!(*last_call.lock(), Some(LastCall::UnbindDevice));
}

#[test]
fn test_bind_from_run_fails_and_resets_to_unlocked() {
    let (mut machine, last_call) = new_machine();
    machine.request_lock_device_resources().unwrap();
    machine.request_start_tdi().unwrap();

    let err = machine.request_lock_device_resources().unwrap_err();
    assert!(matches!(err, TdispGuestOperationError::InvalidDeviceState));
    assert_eq!(machine.state(), TdispTdiState::Unlocked);
    assert_eq!(*last_call.lock(), Some(LastCall::UnbindDevice));
}

#[test]
fn test_start_tdi_from_unlocked_fails_and_resets_to_unlocked() {
    let (mut machine, last_call) = new_machine();

    let err = machine.request_start_tdi().unwrap_err();
    assert!(matches!(err, TdispGuestOperationError::InvalidDeviceState));
    assert_eq!(machine.state(), TdispTdiState::Unlocked);
    assert_eq!(*last_call.lock(), Some(LastCall::UnbindDevice));
}

#[test]
fn test_start_tdi_from_run_fails_and_resets_to_unlocked() {
    let (mut machine, last_call) = new_machine();
    machine.request_lock_device_resources().unwrap();
    machine.request_start_tdi().unwrap();

    let err = machine.request_start_tdi().unwrap_err();
    assert!(matches!(err, TdispGuestOperationError::InvalidDeviceState));
    assert_eq!(machine.state(), TdispTdiState::Unlocked);
    assert_eq!(*last_call.lock(), Some(LastCall::UnbindDevice));
}

// ── Attestation report ────────────────────────────────────────────────────────

#[test]
fn test_attestation_report_from_locked_succeeds() {
    let (mut machine, last_call) = new_machine();
    machine.request_lock_device_resources().unwrap();

    let report = machine
        .request_attestation_report(TdispReportType::InterfaceReport)
        .unwrap();
    assert!(!report.is_empty());
    // State must be unchanged after a successful report retrieval.
    assert_eq!(machine.state(), TdispTdiState::Locked);
    assert_eq!(
        *last_call.lock(),
        Some(LastCall::GetDeviceReport(TdispReportType::InterfaceReport))
    );
}

#[test]
fn test_attestation_report_from_run_succeeds() {
    let (mut machine, last_call) = new_machine();
    machine.request_lock_device_resources().unwrap();
    machine.request_start_tdi().unwrap();

    let report = machine
        .request_attestation_report(TdispReportType::GuestDeviceId)
        .unwrap();
    assert!(!report.is_empty());
    assert_eq!(machine.state(), TdispTdiState::Run);
    assert_eq!(
        *last_call.lock(),
        Some(LastCall::GetDeviceReport(TdispReportType::GuestDeviceId))
    );
}

#[test]
fn test_attestation_report_from_unlocked_fails_and_resets_to_unlocked() {
    let (mut machine, last_call) = new_machine();

    let err = machine
        .request_attestation_report(TdispReportType::InterfaceReport)
        .unwrap_err();
    assert!(matches!(
        err,
        TdispGuestOperationError::InvalidGuestAttestationReportState
    ));
    assert_eq!(machine.state(), TdispTdiState::Unlocked);
    // The state-check failure triggers unbind_all before returning the error.
    assert_eq!(*last_call.lock(), Some(LastCall::UnbindDevice));
}

#[test]
fn test_attestation_report_invalid_type_from_locked_returns_error_without_state_change() {
    let (mut machine, last_call) = new_machine();
    machine.request_lock_device_resources().unwrap();

    // TdispReportType::Invalid is rejected before the host interface is
    // consulted, so the last call must still be the BindDevice from setup.
    let err = machine
        .request_attestation_report(TdispReportType::Invalid)
        .unwrap_err();
    assert!(matches!(
        err,
        TdispGuestOperationError::InvalidGuestAttestationReportType
    ));
    assert_eq!(machine.state(), TdispTdiState::Locked);
    assert_eq!(*last_call.lock(), Some(LastCall::BindDevice));
}
