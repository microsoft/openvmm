// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared vhost-user wire protocol types and async socket I/O.
//!
//! This crate is used by both the vhost-user backend (`vhost_user_backend`)
//! and the vhost-user frontend (`vhost_user_frontend`).

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]
#![expect(missing_docs)]

pub mod protocol;
pub mod socket;

pub use protocol::*;
pub use socket::SocketError;
pub use socket::VhostUserSocket;
