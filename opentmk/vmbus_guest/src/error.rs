// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Typed error surface for [`vmbus_guest`](crate).
//!
//! Per the OpenVMM trust-boundary guidance (see the repository
//! `.github/copilot-instructions.md`), host input is treated as untrusted
//! and never causes a panic; every failing path returns [`Error`] instead.

use crate::protocol::MessageType;
use opentmk::tmkdefs::TmkError;

/// Convenience alias.
pub type Result<T> = core::result::Result<T, Error>;

/// Errors that can be produced by any [`vmbus_guest`](crate) API.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A wire message from the host failed to parse (bad length, unknown
    /// type when a specific type was expected, truncated body, ...).
    #[error("failed to parse vmbus message of type {ty:?}: {reason}")]
    Parse {
        /// The message type we were trying to parse (if known).
        ty: Option<MessageType>,
        /// Human-readable reason.
        reason: &'static str,
    },
    /// The host answered with an unexpected message.
    #[error("unexpected message from host: {0:?}")]
    UnexpectedMessage(MessageType),
    /// Version negotiation walked the whole ladder without agreement.
    #[error("no supported vmbus protocol version")]
    VersionMismatch,
    /// The channel has been rescinded by the host.
    #[error("channel rescinded")]
    Rescinded,
    /// A ring-buffer send was attempted with insufficient space.
    #[error("ring buffer full")]
    RingFull,
    /// A ring-buffer receive found no data.
    #[error("ring buffer empty")]
    RingEmpty,
    /// A completion did not arrive before the bounded retry cap.
    #[error("timed out waiting for host completion")]
    Timeout,
    /// The underlying hypercall failed.
    #[error("hypercall failed")]
    Hypercall(#[from] TmkError),
    /// A completion id was returned that we did not have outstanding.
    #[error("completion for unknown request id")]
    OrphanCompletion,
    /// A path that has not been implemented yet in the scaffold.
    #[error("not implemented")]
    NotImplemented,
}
