// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Error type for the VNC server.

use crate::rfb;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("unsupported protocol version")]
    UnsupportedVersion(rfb::ProtocolVersion),
    #[error("unsupported message type: {0:#x}")]
    UnknownMessage(u8),
    #[error("unsupported qemu message type: {0:#x}")]
    UnknownQemuMessage(u8),
    #[error("unsupported pixel format: {0} bits per pixel")]
    UnsupportedPixelFormat(u8),
    #[error("unsupported security type: {0}")]
    UnsupportedSecurityType(u8),
    #[error("resolution changed but client does not support DesktopSize")]
    ResizeUnsupported,
    #[error("zlib compression failed")]
    ZlibCompression(#[source] flate2::CompressError),
    #[error("socket error")]
    Io(#[from] std::io::Error),
}
