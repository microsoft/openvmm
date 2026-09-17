// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Error type for the VNC server.

use crate::rfb;
use thiserror::Error;

/// Errors that can occur while running the VNC server for a single
/// connection. Returned from `Server::run`.
#[derive(Debug, Error)]
pub enum Error {
    /// Client advertised an RFB version we do not implement.
    #[error("unsupported protocol version")]
    UnsupportedVersion(rfb::ProtocolVersion),
    /// Client sent a top-level message type we do not handle.
    #[error("unsupported message type: {0:#x}")]
    UnknownMessage(u8),
    /// Client sent a QEMU-extension submessage type we do not handle.
    #[error("unsupported qemu message type: {0:#x}")]
    UnknownQemuMessage(u8),
    /// Client requested a pixel format with an unsupported bit depth.
    /// We only support 16 and 32 bits per pixel.
    #[error("unsupported pixel format: {0} bits per pixel")]
    UnsupportedPixelFormat(u8),
    /// Client offered a security type we do not implement (we only support
    /// `None`).
    #[error("unsupported security type: {0}")]
    UnsupportedSecurityType(u8),
    /// The guest framebuffer resolution changed, but the connected client
    /// did not advertise the DesktopSize pseudo-encoding so we cannot tell
    /// it about the change.
    #[error("resolution changed but client does not support DesktopSize")]
    ResizeUnsupported,
    /// The zlib encoder for this connection failed mid-stream.
    #[error("zlib compression failed")]
    ZlibCompression(#[source] flate2::CompressError),
    /// I/O error reading from or writing to the client socket.
    #[error("socket error")]
    Io(#[from] std::io::Error),
}
