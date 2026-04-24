// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::TdispGuestOperationErrorCode;
use thiserror::Error;

/// Error returned by TDISP operations dispatched by the guest.
#[derive(Error, Debug, Copy, Clone)]
#[expect(missing_docs)]
pub enum TdispGuestOperationError {
    #[error("unknown error code")]
    Unknown,
    #[error("the operation was successful")]
    Success,
    #[error("the requested guest protocol type was not valid for this host")]
    InvalidGuestProtocolRequest,
    #[error("the current TDI state is incorrect for this operation")]
    InvalidDeviceState,
    #[error("the reason for this unbind is invalid")]
    InvalidGuestUnbindReason,
    #[error("invalid TDI command ID")]
    InvalidGuestCommandId,
    #[error("operation requested was not implemented")]
    NotImplemented,
    #[error("host failed to process command")]
    HostFailedToProcessCommand,
    #[error(
        "the device was not in the Locked or Run state when the attestation report was requested"
    )]
    InvalidGuestAttestationReportState,
    #[error("invalid attestation report type requested")]
    InvalidGuestAttestationReportType,
}

impl From<TdispGuestOperationErrorCode> for TdispGuestOperationError {
    fn from(err_code: TdispGuestOperationErrorCode) -> Self {
        match err_code {
            TdispGuestOperationErrorCode::Unknown => TdispGuestOperationError::Unknown,
            TdispGuestOperationErrorCode::Success => TdispGuestOperationError::Success,
            TdispGuestOperationErrorCode::InvalidGuestProtocolRequest => {
                TdispGuestOperationError::InvalidGuestProtocolRequest
            }
            TdispGuestOperationErrorCode::InvalidDeviceState => {
                TdispGuestOperationError::InvalidDeviceState
            }
            TdispGuestOperationErrorCode::InvalidGuestUnbindReason => {
                TdispGuestOperationError::InvalidGuestUnbindReason
            }
            TdispGuestOperationErrorCode::InvalidGuestCommandId => {
                TdispGuestOperationError::InvalidGuestCommandId
            }
            TdispGuestOperationErrorCode::NotImplemented => {
                TdispGuestOperationError::NotImplemented
            }
            TdispGuestOperationErrorCode::HostFailedToProcessCommand => {
                TdispGuestOperationError::HostFailedToProcessCommand
            }
            TdispGuestOperationErrorCode::InvalidGuestAttestationReportState => {
                TdispGuestOperationError::InvalidGuestAttestationReportState
            }
            TdispGuestOperationErrorCode::InvalidGuestAttestationReportType => {
                TdispGuestOperationError::InvalidGuestAttestationReportType
            }
        }
    }
}

impl From<TdispGuestOperationError> for TdispGuestOperationErrorCode {
    fn from(err: TdispGuestOperationError) -> Self {
        match err {
            TdispGuestOperationError::Unknown => TdispGuestOperationErrorCode::Unknown,
            TdispGuestOperationError::Success => TdispGuestOperationErrorCode::Success,
            TdispGuestOperationError::InvalidGuestProtocolRequest => {
                TdispGuestOperationErrorCode::InvalidGuestProtocolRequest
            }
            TdispGuestOperationError::InvalidDeviceState => {
                TdispGuestOperationErrorCode::InvalidDeviceState
            }
            TdispGuestOperationError::InvalidGuestUnbindReason => {
                TdispGuestOperationErrorCode::InvalidGuestUnbindReason
            }
            TdispGuestOperationError::InvalidGuestCommandId => {
                TdispGuestOperationErrorCode::InvalidGuestCommandId
            }
            TdispGuestOperationError::NotImplemented => {
                TdispGuestOperationErrorCode::NotImplemented
            }
            TdispGuestOperationError::HostFailedToProcessCommand => {
                TdispGuestOperationErrorCode::HostFailedToProcessCommand
            }
            TdispGuestOperationError::InvalidGuestAttestationReportState => {
                TdispGuestOperationErrorCode::InvalidGuestAttestationReportState
            }
            TdispGuestOperationError::InvalidGuestAttestationReportType => {
                TdispGuestOperationErrorCode::InvalidGuestAttestationReportType
            }
        }
    }
}
