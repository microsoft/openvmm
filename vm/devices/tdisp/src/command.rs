// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::TdispGuestOperationError;
use crate::TdispTdiState;
use open_enum::open_enum;
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

open_enum! {
    /// Represents the command type for a packet sent from the guest to the host or
    /// the response from the host to the guest.
    pub enum TdispCommandId: u64 {
        /// Invalid command id.
        UNKNOWN = 0,

        /// Request the device's TDISP interface information.
        GET_DEVICE_INTERFACE_INFO = 1,

        /// Bind the device to the current partition and transition to Locked.
        BIND = 2,

        /// Get the TDI report for attestation from the host for the device.
        GET_TDI_REPORT = 3,

        /// Transition the device to the Start state after successful attestation.
        START_TDI = 4,

        /// Unbind the device from the partition, reverting it back to the Unlocked state.
        UNBIND = 5,
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
