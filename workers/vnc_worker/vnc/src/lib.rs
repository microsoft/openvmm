// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A VNC server implementation for the RFB (Remote Framebuffer) protocol.
//!
//! Handles a single client connection: RFB handshake (version, security,
//! pixel format), framebuffer update encoding (raw or zlib), and client
//! input (keyboard, mouse, clipboard). Multiple concurrent connections
//! are managed by the worker layer, not this module.
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
//! Two input paths: (1) QEMU extended key events send raw scancodes — the
//! guest layout maps them to characters. (2) Standard keysym events are
//! converted to US scancodes via `scancode.rs` (ASCII 32-126 only; non-ASCII
//! keysyms are dropped).
//!
//! Clipboard paste (Ctrl+Alt+P) types text into the guest. ASCII chars use
//! keysym→scancode; non-ASCII Latin-1 chars (öäü etc.) use the Windows
//! Alt+0+Numpad method (CP-1252). TigerVNC intercepts Ctrl+Alt, so paste
//! only works in RealVNC and noVNC.
//!
//! See `Guide/src/reference/devices/vnc/keyboard.md` for full details.

#![expect(missing_docs)]
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
pub use server::Server;
pub use server::Updater;
pub use traits::Framebuffer;
pub use traits::Input;

use std::sync::Arc;

pub(crate) const TILE_SIZE: u16 = dirty_bitmap::TILE_SIZE;

/// Receiver type for device-reported dirty rectangles. Arc-wrapped to avoid
/// cloning the Vec during per-client broadcast. Backed by `async-channel`
/// (bounded) so the coordinator's broadcast applies backpressure without
/// being able to block — a full channel falls back to a missed-dirty flag.
pub type DirtyRectReceiver = async_channel::Receiver<Arc<Vec<video_core::DirtyRect>>>;
