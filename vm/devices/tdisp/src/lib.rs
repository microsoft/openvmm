// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//!
//! TDISP is a standardized interface for end-to-end encryption and attestation
//! of trusted assigned devices to confidential/isolated partitions. This crate
//! implements structures and interfaces for the host and guest to prepare and
//! assign trusted devices. Examples of technologies that implement TDISP
//! include:
//! - Intel® TDX Connect
//! - AMD® SEV-TIO
//!
//! This crate is primarily used to implement the host side of the guest-to-host
//! interface for TDISP as well as the serialization of guest-to-host commands for both
//! the host and HCL.
//!
//! These structures and interfaces are used by the host virtualization stack
//! to prepare and assign trusted devices to guest partitions.
//!
//! The host is responsible for dispatching guest commands to this machinery by
//! creating a [`TdispHostDeviceTargetEmulator`] and calling through appropriate
//! trait methods to pass guest commands received from the guest to the emulator.
//!
//! This crate will handle incoming guest message structs and manage the state transitions
//! of the TDISP device and ensure valid transitions are made. Once a valid transition is made, the
//! [`TdispHostDeviceTargetEmulator`] will call back into the host through the
//! [`TdispHostDeviceInterface`] trait to allow the host to perform platform actions
//! such as binding the device to a guest partition or retrieving attestation reports.
//! It is the responsibility of the host to provide a [`TdispHostDeviceInterface`]
//! implementation that performs the necessary platform actions.

/// Protobuf serialization of guest commands and responses.
pub mod serialize_proto;

/// Serialization code from PCI standard structures reported from the TDISP device directly.
pub mod devicereport;

#[cfg(test)]
mod tests;

/// Mocks for the host interface and the emulator.
pub mod test_helpers;

use anyhow::Context;
use parking_lot::Mutex;
use std::sync::Arc;
pub use tdisp_proto::GuestToHostCommand;
pub use tdisp_proto::GuestToHostCommandExt;
pub use tdisp_proto::GuestToHostResponse;
pub use tdisp_proto::GuestToHostResponseExt;
pub use tdisp_proto::TdispCommandResponseBind;
pub use tdisp_proto::TdispCommandResponseGetDeviceInterfaceInfo;
pub use tdisp_proto::TdispCommandResponseGetTdiReport;
pub use tdisp_proto::TdispCommandResponseStartTdi;
pub use tdisp_proto::TdispCommandResponseUnbind;
pub use tdisp_proto::TdispDeviceInterfaceInfo;
pub use tdisp_proto::TdispGuestOperationError;
pub use tdisp_proto::TdispGuestOperationErrorCode;
pub use tdisp_proto::TdispGuestProtocolType;
pub use tdisp_proto::TdispGuestUnbindReason;
pub use tdisp_proto::TdispReportType;
pub use tdisp_proto::TdispTdiState;
pub use tdisp_proto::guest_to_host_command::Command;
pub use tdisp_proto::guest_to_host_response::Response;

use tracing::instrument;

/// Callback for receiving TDISP commands from the guest.
pub type TdispCommandCallback = dyn Fn(&GuestToHostCommand) -> anyhow::Result<()> + Send + Sync;

/// Describes the interface that host software should implement to provide TDISP
/// functionality for a device. These interfaces might dispatch to a physical
/// device, or might be implemented by a software emulator.
pub trait TdispHostDeviceInterface: Send + Sync {
    /// Request versioning and protocol negotiation from the host.
    fn tdisp_negotiate_protocol(
        &mut self,
        _requested_guest_protocol: TdispGuestProtocolType,
    ) -> anyhow::Result<TdispDeviceInterfaceInfo>;

    /// Bind a tdi device to the current partition. Transitions device to the Locked
    /// state from Unlocked.
    fn tdisp_bind_device(&mut self) -> anyhow::Result<()>;

    /// Start a bound device by transitioning it to the Run state from the Locked state.
    /// This allows attestation and resources to be accepted into the guest context.
    fn tdisp_start_device(&mut self) -> anyhow::Result<()>;

    /// Unbind a tdi device from the current partition.
    fn tdisp_unbind_device(&mut self) -> anyhow::Result<()>;

    /// Get a device interface report for the device.
    fn tdisp_get_device_report(&mut self, _report_type: TdispReportType)
    -> anyhow::Result<Vec<u8>>;
}

/// Trait added to host virtual devices to dispatch TDISP commands from guests.
pub trait TdispHostDeviceTarget: Send + Sync {
    /// Dispatch a TDISP command from a guest.
    fn tdisp_handle_guest_command(
        &mut self,
        _command: GuestToHostCommand,
    ) -> anyhow::Result<GuestToHostResponse>;
}

/// An emulator which runs the TDISP state machine for a synthetic device.
pub struct TdispHostDeviceTargetEmulator {
    machine: TdispHostStateMachine,
    debug_device_id: String,
}

impl TdispHostDeviceTargetEmulator {
    /// Create a new emulator which runs the TDISP state machine for a synthetic device.
    pub fn new(
        host_interface: Arc<Mutex<dyn TdispHostDeviceInterface>>,
        debug_device_id: &str,
    ) -> Self {
        Self {
            machine: TdispHostStateMachine::new(host_interface),
            debug_device_id: debug_device_id.to_owned(),
        }
    }

    /// Set the debug device ID string.
    pub fn set_debug_device_id(&mut self, debug_device_id: &str) {
        self.machine.set_debug_device_id(debug_device_id.to_owned());
        self.debug_device_id = debug_device_id.to_owned();
    }

    /// Reset the emulator.
    pub fn reset(&self) {}
}

impl TdispHostDeviceTarget for TdispHostDeviceTargetEmulator {
    /// Main entry point for handling a guest command sent to the host.
    /// Dispatches relevant trait interface methods to handle the command.
    /// Formats and returns a response packet.
    #[instrument(fields(device_id = %self.debug_device_id), skip(self))]
    fn tdisp_handle_guest_command(
        &mut self,
        command: GuestToHostCommand,
    ) -> anyhow::Result<GuestToHostResponse> {
        let mut error = TdispGuestOperationError::Success;
        let mut response: Option<Response> = None;
        let state_before = self.machine.state();
        match &command.command {
            Some(Command::GetDeviceInterfaceInfo(req)) => {
                let protocol_type = TdispGuestProtocolType::from_i32(req.guest_protocol_type);

                match protocol_type {
                    Some(protocol_type) => {
                        let interface_info = self.machine.tdisp_negotiate_protocol(protocol_type);
                        match interface_info {
                            Ok(interface_info) => {
                                response = Some(Response::GetDeviceInterfaceInfo(
                                    TdispCommandResponseGetDeviceInterfaceInfo {
                                        interface_info: Some(interface_info),
                                    },
                                ));
                            }
                            Err(err) => {
                                error = err;
                            }
                        }
                    }
                    None => {
                        error = TdispGuestOperationError::InvalidGuestProtocolRequest;
                    }
                }
            }
            Some(Command::Bind(_)) => {
                let bind_res = self.machine.request_lock_device_resources();
                if let Err(err) = bind_res {
                    error = err;
                } else {
                    response = Some(Response::Bind(TdispCommandResponseBind {}));
                }
            }
            Some(Command::StartTdi(_)) => {
                let start_tdi_res = self.machine.request_start_tdi();
                if let Err(err) = start_tdi_res {
                    error = err;
                } else {
                    response = Some(Response::StartTdi(TdispCommandResponseStartTdi {}));
                }
            }
            Some(Command::Unbind(cmd)) => {
                let unbind_reason = TdispGuestUnbindReason::from_i32(cmd.unbind_reason);

                match unbind_reason {
                    Some(reason) => {
                        let unbind_res = self.machine.request_unbind(reason);
                        if let Err(err) = unbind_res {
                            error = err;
                        }
                        response = Some(Response::Unbind(TdispCommandResponseUnbind {}));
                    }
                    None => {
                        error = TdispGuestOperationError::InvalidGuestUnbindReason;
                    }
                }
            }
            Some(Command::GetTdiReport(cmd)) => {
                let report_type = TdispReportType::from_i32(cmd.report_type);
                match report_type {
                    Some(report_type) => {
                        let report_buffer = self.machine.request_attestation_report(report_type);

                        match report_buffer {
                            Ok(report_buffer) => {
                                response = Some(Response::GetTdiReport(
                                    TdispCommandResponseGetTdiReport {
                                        report_type: cmd.report_type,
                                        report_buffer,
                                    },
                                ));
                            }
                            Err(err) => {
                                error = err;
                            }
                        }
                    }
                    None => {
                        error = TdispGuestOperationError::InvalidGuestAttestationReportType;
                    }
                }
            }
            _ => {
                error = TdispGuestOperationError::InvalidGuestCommandId;
            }
        }
        let state_after = self.machine.state();

        match error {
            TdispGuestOperationError::Success => {
                tracing::info!("tdisp_handle_guest_command: Success");
            }
            _ => {
                tracing::error!("tdisp_handle_guest_command: Error: {error:?}");
            }
        }

        let error_code: TdispGuestOperationErrorCode = error.into();
        let resp = GuestToHostResponse {
            result: error_code.into(),
            tdi_state_before: state_before.into(),
            tdi_state_after: state_after.into(),
            response,
        };

        tracing::info!("tdisp_handle_guest_command: response = {resp:?}");

        Ok(resp)
    }
}

/// Trait implemented by TDISP-capable devices on the client side. This includes devices that
/// are assigned to isolated partitions other than the host.
pub trait TdispClientDevice: Send + Sync {
    /// Send a TDISP command to the host for this device.
    /// TODO TDISP: Async? Better handling of device_id in GuestToHostCommand?
    fn tdisp_command_to_host(&self, command: GuestToHostCommand) -> anyhow::Result<()>;
}

/// The number of states to keep in the state history for debug.
const TDISP_STATE_HISTORY_LEN: usize = 10;

/// The reason for an `Unbind` call. This can be guest or host initiated.
/// `Unbind` can be called any time during the assignment flow.
/// This is used for telemetry and debugging.
#[derive(Debug)]
pub enum TdispUnbindReason {
    /// Unknown reason.
    Unknown(anyhow::Error),

    /// The device was unbound manually by the guest or host for a non-error reason.
    GuestInitiated(TdispGuestUnbindReason),

    /// The device attempted to perform an invalid state transition.
    ImpossibleStateTransition(anyhow::Error),

    /// The guest tried to transition the device to the Locked state while the device was not
    /// in the Unlocked state.
    InvalidGuestTransitionToLocked,

    /// The guest tried to transition the device to the Run state while the device was not
    /// in the Locked state.
    InvalidGuestTransitionToRun,

    /// The guest tried to retrieve the attestation report while the device was not in the
    /// Locked or Run state.
    InvalidGuestGetAttestationReportState,

    /// The guest tried to accept the attestation report while the device was not in the
    /// Locked or Run state.
    InvalidGuestAcceptAttestationReportState,

    /// The guest tried to unbind the device while the device with an unbind reason that is
    /// not recognized as a valid guest unbind reason. The unbind still succeeds but the
    /// recorded reason is discarded.
    InvalidGuestUnbindReason(anyhow::Error),
}

/// The state machine for the TDISP assignment flow for a device on the host. Both the guest and host
/// synchronize this state machine with each other as they move through the assignment flow.
pub struct TdispHostStateMachine {
    /// The current state of the TDISP device emulator.
    current_state: TdispTdiState,
    /// A record of the last states the device was in.
    state_history: Vec<TdispTdiState>,
    /// The device ID of the device being assigned.
    debug_device_id: String,
    /// A record of the last unbind reasons for the device.
    unbind_reason_history: Vec<TdispUnbindReason>,
    /// Calls back into the host to perform TDISP actions.
    host_interface: Arc<Mutex<dyn TdispHostDeviceInterface>>,
    /// The guest protocol type that was negotiated with the host interface.
    guest_protocol_type: TdispGuestProtocolType,
}

impl TdispHostStateMachine {
    /// Create a new TDISP state machine with the `Unlocked` state.
    pub fn new(host_interface: Arc<Mutex<dyn TdispHostDeviceInterface>>) -> Self {
        Self {
            current_state: TdispTdiState::Unlocked,
            state_history: Vec::new(),
            debug_device_id: "".to_owned(),
            unbind_reason_history: Vec::new(),
            host_interface,
            guest_protocol_type: TdispGuestProtocolType::Invalid,
        }
    }

    /// Set the debug device ID string.
    pub fn set_debug_device_id(&mut self, debug_device_id: String) {
        self.debug_device_id = debug_device_id;
    }

    /// Get the current state of the TDI.
    fn state(&self) -> TdispTdiState {
        self.current_state
    }

    fn ensure_negotiated_protocol(&self) -> anyhow::Result<()> {
        if self.guest_protocol_type == TdispGuestProtocolType::Invalid {
            tracing::error!(
                "Guest tried to perform a state transition without negotiating a protocol with the host!"
            );
            return Err(anyhow::anyhow!(
                "Guest tried to perform a state transition without negotiating a protocol with the host!"
            ));
        }
        Ok(())
    }

    /// Check if the state machine can transition to the new state. This protects the underlying state machinery
    /// while higher level transition machinery tries to avoid these conditions. If the new state is impossible,
    /// `false` is returned.
    #[instrument(fields(device_id = %self.debug_device_id), skip(self))]
    fn is_valid_state_transition(&self, new_state: &TdispTdiState) -> bool {
        // All state machine transitions are specifically denied until the host as negotiated a protocol.
        match self.ensure_negotiated_protocol() {
            Ok(_) => {}
            Err(e) => {
                tracing::error!("Failed to transition state: {e:?}");
                return false;
            }
        }

        match (self.current_state, *new_state) {
            // Valid forward progress states from Unlocked -> Run
            (TdispTdiState::Unlocked, TdispTdiState::Locked) => true,
            (TdispTdiState::Locked, TdispTdiState::Run) => true,

            // Device can always return to the Unlocked state with `Unbind`
            (TdispTdiState::Run, TdispTdiState::Unlocked) => true,
            (TdispTdiState::Locked, TdispTdiState::Unlocked) => true,
            (TdispTdiState::Unlocked, TdispTdiState::Unlocked) => true,

            // Every other state transition is invalid
            _ => false,
        }
    }

    /// Transitions the state machine to the new state if it is valid. If the new state is invalid,
    /// the state of the device is reset to the `Unlocked` state.
    #[instrument(fields(device_id = %self.debug_device_id), skip(self))]
    fn transition_state_to(&mut self, new_state: TdispTdiState) -> anyhow::Result<()> {
        tracing::info!(
            "Request to transition from {:?} -> {:?}",
            self.current_state,
            new_state
        );

        // Ensure the state transition is valid
        if !self.is_valid_state_transition(&new_state) {
            tracing::info!(
                "Invalid state transition {:?} -> {:?}",
                self.current_state,
                new_state
            );
            return Err(anyhow::anyhow!(
                "Invalid state transition {:?} -> {:?}",
                self.current_state,
                new_state
            ));
        }

        // Record the state history
        if self.state_history.len() == TDISP_STATE_HISTORY_LEN {
            self.state_history.remove(0);
        }
        self.state_history.push(self.current_state);

        // Transition to the new state
        self.current_state = new_state;
        tracing::info!("Transitioned to {:?}", self.current_state);

        Ok(())
    }

    /// Transition the device to the `Unlocked` state regardless of the current state.
    #[instrument(fields(device_id = %self.debug_device_id), skip(self))]
    fn unbind_all(&mut self, reason: TdispUnbindReason) -> anyhow::Result<()> {
        tracing::info!("Unbind called with reason {:?}", reason);

        // All states can be reset to the Unlocked state. This can only happen if the
        // state is corrupt beyond the state machine.
        if let Err(reason) = self.transition_state_to(TdispTdiState::Unlocked) {
            return Err(anyhow::anyhow!(
                "Impossible state machine violation during TDISP Unbind: {:?}",
                reason
            ));
        }

        // Call back into the host to bind the device.
        let res = self
            .host_interface
            .lock()
            .tdisp_unbind_device()
            .context("host failed to unbind TDI");

        if let Err(e) = res {
            tracing::error!("Failed to unbind TDI: {:?}", e);
            return Err(e);
        }

        // Record the unbind reason
        if self.unbind_reason_history.len() == TDISP_STATE_HISTORY_LEN {
            self.unbind_reason_history.remove(0);
        }
        self.unbind_reason_history.push(reason);

        Ok(())
    }
}

/// Represents an interface by which guest commands can be dispatched to a
/// backing TDISP state handler in the host. This could be an emulated TDISP device or an
/// assigned TDISP device that is actually connected to the guest.
pub trait TdispGuestRequestInterface {
    /// Before a guest can communicate with the host, the guest must negotiate a
    /// protocol with the host. This is done by calling this function with the
    /// guest's desired protocol type. The host responds with the protocol that
    /// it will use to communicate with the guest and includes information about
    /// the TDISP capabilities of the device.
    ///
    /// If the host reports that this device not TDISP capable,
    /// [`TdispDeviceInterfaceInfo::guest_protocol_type`] will be
    /// [`TdispGuestProtocolType::Invalid`].
    fn tdisp_negotiate_protocol(
        &mut self,
        requested_guest_protocol: TdispGuestProtocolType,
    ) -> Result<TdispDeviceInterfaceInfo, TdispGuestOperationError>;

    /// Transition the device from the Unlocked to Locked state. This takes place after the
    /// device has been assigned to the guest partition and the resources for the device have
    /// been configured by the guest by not yet validated.
    /// The device will in the `Locked` state can still perform unencrypted operations until it has
    /// been transitioned to the `Run` state. The device will be attested and moved to the `Run` state.
    ///
    /// Attempting to transition the device to the `Locked` state while the device is not in the
    /// `Unlocked` state will cause an error and unbind the device.
    fn request_lock_device_resources(&mut self) -> Result<(), TdispGuestOperationError>;

    /// Transition the device from the Locked to the Run state. This takes place after the
    /// device has been assigned resources and the resources have been locked to the guest.
    /// The device will then transition to the `Run` state, where it will be non-functional
    /// until the guest undergoes attestation and resources are accepted into the guest context.
    ///
    /// Attempting to transition the device to the `Run` state while the device is not in the
    /// `Locked` state will cause an error and unbind the device.
    fn request_start_tdi(&mut self) -> Result<(), TdispGuestOperationError>;

    /// Retrieves the attestation report for the device when the device is in the `Locked` or
    /// `Run` state. The device resources will not be functional until the
    /// resources have been accepted into the guest while the device is in the
    /// `Run` state.
    ///
    /// Attempting to retrieve the attestation report while the device is not in
    /// the `Locked` or `Run` state will cause an error and unbind the device.
    fn request_attestation_report(
        &mut self,
        report_type: TdispReportType,
    ) -> Result<Vec<u8>, TdispGuestOperationError>;

    /// Guest initiates a graceful unbind of the device. The guest might
    /// initiate an unbind for a variety of reasons:
    ///  - Device is being detached/deactivated and is no longer needed in a functional state
    ///  - Device is powering down or entering a reset
    ///
    /// The device will transition to the `Unlocked` state. The guest can call
    /// this function at any time in any state to reset the device to the
    /// `Unlocked` state.
    fn request_unbind(
        &mut self,
        reason: TdispGuestUnbindReason,
    ) -> Result<(), TdispGuestOperationError>;
}

impl TdispGuestRequestInterface for TdispHostStateMachine {
    /// Request versioning and protocol negotiation from the host.
    #[instrument(fields(device_id = %self.debug_device_id), skip(self))]
    fn tdisp_negotiate_protocol(
        &mut self,
        requested_guest_protocol: TdispGuestProtocolType,
    ) -> Result<TdispDeviceInterfaceInfo, TdispGuestOperationError> {
        if self.guest_protocol_type != TdispGuestProtocolType::Invalid {
            tracing::error!(
                "Guest tried to negotiate a protocol with the host while a protocol was already negotiated!"
            );
            return Err(TdispGuestOperationError::InvalidGuestProtocolRequest);
        }

        if requested_guest_protocol == TdispGuestProtocolType::Invalid {
            tracing::error!("Guest tried to negotiate Invalid as a protocol");
            return Err(TdispGuestOperationError::InvalidGuestProtocolRequest);
        }

        // Call back into the host to negotiate protocol information.
        let res = self
            .host_interface
            .lock()
            .tdisp_negotiate_protocol(requested_guest_protocol)
            .context("failed to call to negotiate protocol");

        match res {
            Ok(interface_info) => {
                match TdispGuestProtocolType::from_i32(interface_info.guest_protocol_type) {
                    Some(guest_protocol_type) => {
                        if guest_protocol_type == TdispGuestProtocolType::Invalid {
                            tracing::error!(
                                "Guest protocol negotiated with invalid value: {guest_protocol_type:?}"
                            );
                            Err(TdispGuestOperationError::InvalidGuestProtocolRequest)
                        } else {
                            self.guest_protocol_type = guest_protocol_type;
                            tracing::info!(
                                "Guest protocol negotiated successfully to: {:?}",
                                interface_info
                            );
                            Ok(interface_info)
                        }
                    }
                    None => {
                        tracing::error!(
                            "Guest protocol negotiated with none value: {interface_info:?}"
                        );
                        Err(TdispGuestOperationError::InvalidGuestProtocolRequest)
                    }
                }
            }
            Err(e) => {
                tracing::error!("Failed to negotiate protocol with host interface: {e:?}");
                Err(TdispGuestOperationError::HostFailedToProcessCommand)
            }
        }
    }

    #[instrument(fields(device_id = %self.debug_device_id), skip(self))]
    fn request_lock_device_resources(&mut self) -> Result<(), TdispGuestOperationError> {
        // Ensure the guest protocol is negotiated.
        self.ensure_negotiated_protocol()
            .map_err(|_| TdispGuestOperationError::InvalidDeviceState)?;

        // If the guest attempts to transition the device to the Locked state while the device
        // is not in the Unlocked state, the device is reset to the Unlocked state.
        if self.current_state != TdispTdiState::Unlocked {
            tracing::error!(
                "Unlocked to Locked state called while device was not in Unlocked state."
            );

            self.unbind_all(TdispUnbindReason::InvalidGuestTransitionToLocked)
                .map_err(|_| TdispGuestOperationError::HostFailedToProcessCommand)?;
            return Err(TdispGuestOperationError::InvalidDeviceState);
        }

        tracing::info!("Device bind requested, trying to transition from Unlocked to Locked state");

        // Call back into the host to bind the device.
        let res = self
            .host_interface
            .lock()
            .tdisp_bind_device()
            .context("failed to call to bind TDI");

        if let Err(e) = res {
            tracing::error!("Failed to bind TDI: {e:?}");
            return Err(TdispGuestOperationError::HostFailedToProcessCommand);
        }

        tracing::info!("Device transition from Unlocked to Locked state");
        match self.transition_state_to(TdispTdiState::Locked) {
            Ok(_) => {}
            Err(e) => {
                tracing::error!("Failed to transition to Locked state: {e:?}");
                return Err(TdispGuestOperationError::HostFailedToProcessCommand);
            }
        }
        Ok(())
    }

    #[instrument(fields(device_id = %self.debug_device_id), skip(self))]
    fn request_start_tdi(&mut self) -> Result<(), TdispGuestOperationError> {
        // Ensure the guest protocol is negotiated.
        self.ensure_negotiated_protocol()
            .map_err(|_| TdispGuestOperationError::InvalidDeviceState)?;

        if self.current_state != TdispTdiState::Locked {
            tracing::error!("StartTDI called while device was not in Locked state.");
            self.unbind_all(TdispUnbindReason::InvalidGuestTransitionToRun)
                .map_err(|_| TdispGuestOperationError::HostFailedToProcessCommand)?;

            return Err(TdispGuestOperationError::InvalidDeviceState);
        }

        tracing::info!("Device start requested, trying to transition from Locked to Run state");

        // Call back into the host to bind the device.
        let res = self
            .host_interface
            .lock()
            .tdisp_start_device()
            .context("failed to call to start TDI");

        if let Err(e) = res {
            tracing::error!("Failed to start TDI: {e:?}");
            return Err(TdispGuestOperationError::HostFailedToProcessCommand);
        }

        tracing::info!("Device transition from Locked to Run state");
        match self.transition_state_to(TdispTdiState::Run) {
            Ok(_) => {}
            Err(e) => {
                tracing::error!("Failed to transition to Run state: {e:?}");
                return Err(TdispGuestOperationError::HostFailedToProcessCommand);
            }
        }

        Ok(())
    }

    #[instrument(fields(device_id = %self.debug_device_id), skip(self))]
    fn request_attestation_report(
        &mut self,
        report_type: TdispReportType,
    ) -> Result<Vec<u8>, TdispGuestOperationError> {
        // Ensure the guest protocol is negotiated.
        self.ensure_negotiated_protocol()
            .map_err(|_| TdispGuestOperationError::InvalidDeviceState)?;

        if self.current_state != TdispTdiState::Locked && self.current_state != TdispTdiState::Run {
            tracing::error!(
                "Request to retrieve attestation report called while device was not in Locked or Run state."
            );
            self.unbind_all(TdispUnbindReason::InvalidGuestGetAttestationReportState)
                .map_err(|_| TdispGuestOperationError::HostFailedToProcessCommand)?;

            return Err(TdispGuestOperationError::InvalidGuestAttestationReportState);
        }

        if report_type == TdispReportType::Invalid {
            tracing::error!("Invalid report type TdispReportId::INVALID requested");
            return Err(TdispGuestOperationError::InvalidGuestAttestationReportType);
        }

        let report_buffer = self
            .host_interface
            .lock()
            .tdisp_get_device_report(report_type)
            .context("failed to call to get device report from host");

        match report_buffer {
            Ok(report_buffer) => {
                tracing::info!("Retrieve attestation report called successfully");
                Ok(report_buffer)
            }
            Err(e) => {
                tracing::error!("Failed to get device report from host: {e:?}");
                Err(TdispGuestOperationError::HostFailedToProcessCommand)
            }
        }
    }

    #[instrument(fields(device_id = %self.debug_device_id), skip(self))]
    fn request_unbind(
        &mut self,
        reason: TdispGuestUnbindReason,
    ) -> Result<(), TdispGuestOperationError> {
        // Ensure the guest protocol is negotiated.
        self.ensure_negotiated_protocol()
            .map_err(|_| TdispGuestOperationError::InvalidDeviceState)?;

        // The guest can provide a reason for the unbind. If the unbind reason isn't valid for a guest (such as
        // if the guest says it is unbinding due to a host-related error), the reason is discarded and InvalidGuestUnbindReason
        // is recorded in the unbind history.
        let reason = match reason {
            TdispGuestUnbindReason::Graceful => TdispUnbindReason::GuestInitiated(reason),
            _ => {
                tracing::error!(
                    "Invalid guest unbind reason {} requested",
                    reason.as_str_name()
                );
                TdispUnbindReason::InvalidGuestUnbindReason(anyhow::anyhow!(
                    "Invalid guest unbind reason {} requested",
                    reason.as_str_name()
                ))
            }
        };

        tracing::info!(
            "Guest request to unbind succeeds while device is in {:?} (reason: {:?})",
            self.current_state,
            reason
        );

        self.unbind_all(reason)
            .map_err(|_| TdispGuestOperationError::HostFailedToProcessCommand)?;

        Ok(())
    }
}
