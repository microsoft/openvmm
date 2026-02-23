// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TDISP guest-to-host command protocol definitions.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

// Crates used by generated code. Reference them explicitly to ensure that
// automated tools do not remove them.
use crate::guest_to_host_command::Command;
use crate::guest_to_host_response::Response;
use inspect as _;
use prost as _;
mod errorcode;
pub use errorcode::*;

include!(concat!(env!("OUT_DIR"), "/tdisp.rs"));

pub trait GuestToHostCommandExt {
    /// Returns the command type name of the command.
    fn get_type_name(&self) -> Option<&str>;
}

impl GuestToHostCommandExt for GuestToHostCommand {
    fn get_type_name(&self) -> Option<&str> {
        match self.command {
            Some(Command::GetDeviceInterfaceInfo(_)) => Some("GetDeviceInterfaceInfo"),
            Some(Command::Bind(_)) => Some("Bind"),
            Some(Command::StartTdi(_)) => Some("StartTdi"),
            Some(Command::Unbind(_)) => Some("Unbind"),
            Some(Command::GetTdiReport(_)) => Some("GetTdiReport"),
            None => None,
        }
    }
}

/// Implemented by each response payload type so that [`GuestToHostResponseExt::get_response`]
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

/// Provides helper methods for common operations on [`GuestToHostResponse`].
pub trait GuestToHostResponseExt {
    /// Returns the error code of the response, if any.
    fn get_error_code(&self) -> Option<TdispGuestOperationErrorCode>;

    /// Returns the packet type name of the response.
    fn get_type_name(&self) -> Option<&str>;

    /// Consumes the response and returns the inner payload if the result is
    /// [`TdispGuestOperationError::Success`] and the oneof variant matches `T`.
    /// Returns the error code otherwise.
    ///
    /// # Example
    /// ```ignore
    /// let bind = resp.get_response::<TdispCommandResponseBind>()?;
    /// ```
    fn get_response<T: GuestToHostResponseVariant>(self) -> Result<T, TdispGuestOperationError>;
}

impl GuestToHostResponseExt for GuestToHostResponse {
    fn get_error_code(&self) -> Option<TdispGuestOperationErrorCode> {
        TdispGuestOperationErrorCode::from_i32(self.result)
    }

    fn get_type_name(&self) -> Option<&str> {
        match self.response {
            Some(Response::GetDeviceInterfaceInfo(_)) => Some("GetDeviceInterfaceInfo"),
            Some(Response::Bind(_)) => Some("Bind"),
            Some(Response::StartTdi(_)) => Some("StartTdi"),
            Some(Response::Unbind(_)) => Some("Unbind"),
            Some(Response::GetTdiReport(_)) => Some("GetTdiReport"),
            None => None,
        }
    }

    fn get_response<T: GuestToHostResponseVariant>(self) -> Result<T, TdispGuestOperationError> {
        match self.get_error_code() {
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
