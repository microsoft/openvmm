// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared vhost-user wire protocol types and async socket I/O.
//!
//! This crate is used by both the vhost-user backend (`vhost_user_backend`)
//! and the vhost-user frontend (`vhost_user_frontend`).

#![cfg(target_os = "linux")]
#![expect(missing_docs)]
// UNSAFETY: socket.rs uses libc sendmsg/recvmsg and handling cmsg ancillary data.
#![expect(unsafe_code)]

pub mod protocol;
pub mod socket;

pub use protocol::*;
pub use socket::SocketError;
pub use socket::VhostUserSocket;
