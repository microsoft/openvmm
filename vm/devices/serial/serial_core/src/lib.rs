// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core types shared by serial port implementations and users.

pub mod disconnected;
pub mod resources;
pub mod serial_io;

use futures::io::AsyncRead;
use futures::io::AsyncWrite;
use inspect::InspectMut;
use std::task::Context;
use std::task::Poll;

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
}
