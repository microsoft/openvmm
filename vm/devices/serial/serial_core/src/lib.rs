// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core types shared by serial port implementations and users.

#![forbid(unsafe_code)]

pub mod debugger;
pub mod disconnected;
pub mod resources;
pub mod serial_io;

use futures::io::AsyncRead;
use futures::io::AsyncWrite;
use inspect::InspectMut;
use mesh::MeshPayload;
use std::task::Context;
use std::task::Poll;

/// Authenticated operating-system identity of a local serial peer.
#[derive(Clone, Debug, Eq, PartialEq, MeshPayload)]
pub enum LocalPeerIdentity {
    /// Effective Unix user ID obtained from the connected socket.
    UnixUid(u32),
    /// Windows security identifier in its self-relative binary representation.
    WindowsSid {
        /// Binary SID bytes, zero-padded to the maximum SID size.
        bytes: [u8; 68],
        /// Number of meaningful bytes in `bytes`.
        length: u8,
    },
    /// No peer-identity implementation exists for this platform.
    Unsupported,
}

impl LocalPeerIdentity {
    /// Constructs a validated Windows SID identity.
    pub fn windows_sid(bytes: [u8; 68], length: u8) -> std::io::Result<Self> {
        if !(8..=68).contains(&length) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Windows SID length is outside the supported range",
            ));
        }
        Ok(Self::WindowsSid { bytes, length })
    }
}

/// Trait for types providing serial IO.
pub trait SerialIo: AsyncRead + AsyncWrite + Send + InspectMut + Unpin {
    /// Returns true if the backend is already connected.
    fn is_connected(&self) -> bool;

    /// Polls for the serial backend to connect.
    ///
    /// When the serial backend disconnects, [`AsyncRead::poll_read`] should
    /// return `Ok(0)`.
    fn poll_connect(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>>;

    /// Polls for the serial backend to disconnect.
    fn poll_disconnect(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>>;

    /// Returns the authenticated local identity of the connected peer.
    ///
    /// Generic serial backends do not promise peer authentication. Control
    /// endpoints must reject `None`.
    fn local_peer_identity(&self) -> std::io::Result<Option<LocalPeerIdentity>> {
        Ok(None)
    }

    /// Closes the current connection while preserving a reconnectable listener.
    ///
    /// Control endpoints use this to evict unauthenticated clients. Generic
    /// serial backends may return [`std::io::ErrorKind::Unsupported`].
    fn disconnect_current(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "serial backend cannot actively disconnect its peer",
        ))
    }
}
