// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::TdispGuestOperationError;
use crate::TdispTdiState;
use std::fmt::Display;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Represents a TDISP command sent from the guest to the host.
#[derive(Debug, Copy, Clone)]
pub struct GuestToHostCommand {
    /// Device ID of the target device.
    pub device_id: u64,
    /// The command ID.
    pub command_id: TdispCommandId,
    /// The payload of the command if it has one.
    pub payload: TdispCommandRequestPayload,
}

/// Represents a response from a TDISP command sent to the host by a guest.
#[derive(Debug, Clone)]
pub struct GuestToHostResponse {
    /// The command ID.
    pub command_id: TdispCommandId,
    /// The result status of the command.
    pub result: TdispGuestOperationError,
    /// The state of the TDI before the command was executed.
    pub tdi_state_before: TdispTdiState,
    /// The state of the TDI after the command was executed.
    pub tdi_state_after: TdispTdiState,
    /// The payload of the response if it has one.
    pub payload: TdispCommandResponsePayload,
}

impl Display for GuestToHostCommand {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Display the Debug representation of the command.
        f.debug_struct("GuestToHostCommand")
            .field("command_id", &self.command_id)
            .finish()
    }
}

/// Represents a TDISP command sent from the guest to the host.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum TdispCommandId {
    /// Invalid command id.
    Unknown,

    /// Request the device's TDISP interface information.
    GetDeviceInterfaceInfo,

    /// Bind the device to the current partition and transition to Locked.
    Bind,

    /// Get the TDI report for attestation from the host for the device.
    GetTdiReport,

    /// Transition the device to the Start state after successful attestation.
    StartTdi,

    /// Unbind the device from the partition, reverting it back to the Unlocked state.
    Unbind,
}

impl From<TdispCommandId> for u64 {
    fn from(value: TdispCommandId) -> Self {
        match value {
            TdispCommandId::Unknown => 0,
            TdispCommandId::GetDeviceInterfaceInfo => 1,
            TdispCommandId::Bind => 2,
            TdispCommandId::GetTdiReport => 3,
            TdispCommandId::StartTdi => 4,
            TdispCommandId::Unbind => 5,
        }
    }
}

impl From<u64> for TdispCommandId {
    fn from(value: u64) -> Self {
        match value {
            0 => TdispCommandId::Unknown,
            1 => TdispCommandId::GetDeviceInterfaceInfo,
            2 => TdispCommandId::Bind,
            3 => TdispCommandId::GetTdiReport,
            4 => TdispCommandId::StartTdi,
            5 => TdispCommandId::Unbind,
            _ => TdispCommandId::Unknown,
        }
    }
}

/// Represents the TDISP device interface information, such as the version and supported features.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct TdispDeviceInterfaceInfo {
    /// The major version for the interface. This does not necessarily match to a TDISP specification version.
    /// [TDISP TODO] dead_code
    pub interface_version_major: u32,

    /// The minor version for the interface. This does not necessarily match to a TDISP specification version.
    /// [TDISP TODO] dead_code
    pub interface_version_minor: u32,

    /// [TDISP TODO] Placeholder for bitfield advertising feature set capabilities.
    pub supported_features: u64,

    /// Device ID used to communicate with firmware for this particular device.
    pub tdisp_device_id: u64,
}

/// Serialized to and from the payload field of a TdispCommandResponse
#[derive(Debug, Clone)]
pub enum TdispCommandResponsePayload {
    /// No payload.
    None,

    /// TdispCommandId::GetDeviceInterfaceInfo
    GetDeviceInterfaceInfo(TdispDeviceInterfaceInfo),

    /// TdispCommandId::GetTdiReport
    GetTdiReport(TdispCommandResponseGetTdiReport),
}

impl From<TdispDeviceInterfaceInfo> for TdispCommandResponsePayload {
    fn from(value: TdispDeviceInterfaceInfo) -> Self {
        TdispCommandResponsePayload::GetDeviceInterfaceInfo(value)
    }
}

/// Serialized to and from the payload field of a TdispCommandRequest
#[derive(Debug, Copy, Clone)]
pub enum TdispCommandRequestPayload {
    /// No payload.
    None,

    /// TdispCommandId::Unbind
    Unbind(TdispCommandRequestUnbind),

    /// TdispCommandId::GetTdiReport
    GetTdiReport(TdispCommandRequestGetTdiReport),
}

/// Represents a request to unbind the device back to the Unlocked state.
#[derive(Debug, Copy, Clone, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct TdispCommandRequestUnbind {
    /// The reason for the unbind. See: `TdispGuestUnbindReason`
    pub unbind_reason: u64,
}

/// Represents a request to get a specific device report form the TDI.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct TdispCommandRequestGetTdiReport {
    /// The type of report to request.
    /// See: `TdispDeviceReportType``
    pub report_type: u32,
}

/// Represents the payload of the resposne for a TdispCommandId::GetTdiReport.
#[derive(Debug, Clone)]
pub struct TdispCommandResponseGetTdiReport {
    /// The type of report requested.
    /// See: `TdispDeviceReportType``
    pub report_type: u32,

    /// The buffer containing the requested report.
    pub report_buffer: Vec<u8>,
}

/// Represents the serialized form of a TdispCommandRequestGetTdiReport.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct TdispSerializedCommandRequestGetTdiReport {
    /// The type of report to request. See: `TdispDeviceReportType``
    pub report_type: u32,

    /// The size of the report buffer.
    pub report_buffer_size: u32,
    // The remainder of the `report_buffer_size` bytes to follow are the bytes of the returned report.
}
