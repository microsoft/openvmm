// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A VNC server implementation for the RFB (Remote Framebuffer) protocol.
//!
//! Handles a single client connection: RFB handshake (version, security,
//! pixel format), framebuffer update encoding (raw, zlib, or ZRLE), and client
//! input (keyboard, mouse, clipboard). Concurrent connections are managed by
//! the worker layer, not this module.
//!
//! # Components
//!
//! - `Encoder`: per-connection zlib state and pixel format conversion.
//! - `UpdateState`: framebuffer snapshots and dirty detection (device rects
//!   or tile diff fallback).
//! - `ClientState`: per-connection mutable state (format, resolution, flags).
//! - [`Server`]: ties the above together with the RFB protocol state machine.
//!
//! # Keyboard Handling
//!
//! Two input paths: (1) QEMU extended key events send raw scancodes, mapped to
//! characters by the guest layout. (2) Standard keysym events are converted to
//! US scancodes via `scancode.rs` (ASCII 32-126 only; non-ASCII keysyms are
//! dropped).
//!
//! Clipboard paste (Ctrl+Alt+P) types text into the guest: ASCII chars via
//! keysym to scancode, non-ASCII Latin-1 chars (öäü etc.) via the Windows
//! Alt+0+Numpad method (CP-1252).
//!
//! See `Guide/src/reference/devices/vnc/keyboard.md` for full details.

#![forbid(unsafe_code)]

mod dirty_bitmap;
mod encoder;
mod error;
mod pixel;
mod rfb;
mod scancode;
mod server;
mod traits;
mod update_state;

pub use dirty_bitmap::DirtyBitmap;
pub use dirty_bitmap::Rect;
pub use error::Error;
pub use rfb::EncodingType;
pub use server::Server;
pub use server::TileSizeMode;
pub use server::Updater;
pub use traits::Framebuffer;
pub use traits::Input;

use std::sync::Arc;

/// Receiver type for device-reported dirty rectangles. Bounded `async-channel`;
/// a full channel falls back to a missed-dirty flag.
pub type DirtyRectReceiver = async_channel::Receiver<Arc<Vec<video_core::DirtyRect>>>;
