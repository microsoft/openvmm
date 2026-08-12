// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TDISP guest-to-host command protocol definitions.

#![expect(missing_docs)]
#![forbid(unsafe_code)]
#![expect(
    unused_qualifications,
    reason = "generated code contains fully qualified paths"
)]
#![expect(clippy::allow_attributes)]

// Crates used by generated code. Reference them explicitly to ensure that
// automated tools do not remove them.
use inspect as _;
use prost as _;

mod errorcode;
pub use errorcode::*;

use crate::guest_to_host_command::Command;
use crate::guest_to_host_response::Response;
use inspect::Inspect;
use std::fmt::Display;

include!(concat!(env!("OUT_DIR"), "/tdisp.rs"));

pub trait GuestToHostCommandExt {
    /// Returns the command type name of the command.
    fn type_name(&self) -> Option<&str>;
}

impl GuestToHostCommandExt for GuestToHostCommand {
    fn type_name(&self) -> Option<&str> {
        match self.command {
            Some(Command::GetDeviceInterfaceInfo(_)) => Some("GetDeviceInterfaceInfo"),
            Some(Command::Bind(_)) => Some("Bind"),
            Some(Command::StartTdi(_)) => Some("StartTdi"),
            Some(Command::Unbind(_)) => Some("Unbind"),
            Some(Command::GetTdiReport(_)) => Some("GetTdiReport"),
            Some(Command::AcceptPrivateMmioRange(_)) => Some("AcceptPrivateMmioRange"),
            None => None,
        }
    }
}

/// Implemented by each response payload type so that [`GuestToHostResponseExt::response`]
/// can extract it generically from a [`Response`] oneof variant.
pub trait GuestToHostResponseVariant: Sized {
    fn from_response_variant(response: Response) -> Option<Self>;
}

impl GuestToHostResponseVariant for TdispCommandResponseGetDeviceInterfaceInfo {
    fn from_response_variant(response: Response) -> Option<Self> {
        match response {
            Response::GetDeviceInterfaceInfo(r) => Some(r),
            _ => None,
        }
    }
}

impl GuestToHostResponseVariant for TdispCommandResponseBind {
    fn from_response_variant(response: Response) -> Option<Self> {
        match response {
            Response::Bind(r) => Some(r),
            _ => None,
        }
    }
}

impl GuestToHostResponseVariant for TdispCommandResponseGetTdiReport {
    fn from_response_variant(response: Response) -> Option<Self> {
        match response {
            Response::GetTdiReport(r) => Some(r),
            _ => None,
        }
    }
}

impl GuestToHostResponseVariant for TdispCommandResponseStartTdi {
    fn from_response_variant(response: Response) -> Option<Self> {
        match response {
            Response::StartTdi(r) => Some(r),
            _ => None,
        }
    }
}

impl GuestToHostResponseVariant for TdispCommandResponseUnbind {
    fn from_response_variant(response: Response) -> Option<Self> {
        match response {
            Response::Unbind(r) => Some(r),
            _ => None,
        }
    }
}

impl GuestToHostResponseVariant for TdispCommandResponseAcceptPrivateMmioRange {
    fn from_response_variant(response: Response) -> Option<Self> {
        match response {
            Response::AcceptPrivateMmioRange(r) => Some(r),
            _ => None,
        }
    }
}

/// Provides helper methods for common operations on [`GuestToHostResponse`].
pub trait GuestToHostResponseExt {
    /// Returns the error code of the response, if any.
    fn error_code(&self) -> Option<TdispGuestOperationErrorCode>;

    /// Returns the packet type name of the response.
    fn type_name(&self) -> Option<&str>;

    /// Consumes the response and returns the inner payload if the result is
    /// [`TdispGuestOperationError::Success`] and the oneof variant matches `T`.
    /// Returns the error code otherwise.
    ///
    /// # Example
    /// ```ignore
    /// let bind = resp.response::<TdispCommandResponseBind>()?;
    /// ```
    fn response<T: GuestToHostResponseVariant>(self) -> Result<T, TdispGuestOperationError>;

    /// Returns the TDI state of the device before the command was processed, if available.
    fn tdi_state_before_enum(&self) -> Option<TdispTdiState>;

    /// Returns the TDI state of the device after the command was processed, if available.
    fn tdi_state_after_enum(&self) -> Option<TdispTdiState>;
}

impl GuestToHostResponseExt for GuestToHostResponse {
    fn error_code(&self) -> Option<TdispGuestOperationErrorCode> {
        TdispGuestOperationErrorCode::from_i32(self.result)
    }

    fn type_name(&self) -> Option<&str> {
        match self.response {
            Some(Response::GetDeviceInterfaceInfo(_)) => Some("GetDeviceInterfaceInfo"),
            Some(Response::Bind(_)) => Some("Bind"),
            Some(Response::StartTdi(_)) => Some("StartTdi"),
            Some(Response::Unbind(_)) => Some("Unbind"),
            Some(Response::GetTdiReport(_)) => Some("GetTdiReport"),
            Some(Response::AcceptPrivateMmioRange(_)) => Some("AcceptPrivateMmioRange"),
            None => None,
        }
    }

    fn tdi_state_before_enum(&self) -> Option<TdispTdiState> {
        TdispTdiState::from_i32(self.tdi_state_before)
    }

    fn tdi_state_after_enum(&self) -> Option<TdispTdiState> {
        TdispTdiState::from_i32(self.tdi_state_after)
    }

    fn response<T: GuestToHostResponseVariant>(self) -> Result<T, TdispGuestOperationError> {
        match self.error_code() {
            Some(TdispGuestOperationErrorCode::Success) => {
                match self.response.and_then(T::from_response_variant) {
                    Some(r) => Ok(r),
                    None => Err(TdispGuestOperationErrorCode::Unknown.into()),
                }
            }
            Some(err) => Err(err.into()),
            None => Err(TdispGuestOperationErrorCode::Unknown.into()),
        }
    }
}

impl Display for TdispTdiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state_str = match self {
            TdispTdiState::Uninitialized => "Uninitialized",
            TdispTdiState::Unlocked => "Unlocked",
            TdispTdiState::Locked => "Locked",
            TdispTdiState::Run => "Run",
        };
        write!(f, "{}", state_str)
    }
}

impl Inspect for TdispTdiState {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.value(format!("{}", self));
    }
}
