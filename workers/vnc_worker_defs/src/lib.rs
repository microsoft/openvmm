// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Public worker IDs and parameter types for launching the VNC worker.
//!
//! This crate is the shared boundary between the VNC worker process and its
//! launcher. Two worker variants exist: a TCP listener and a vmsocket listener.

#![forbid(unsafe_code)]

use mesh::MeshPayload;
use mesh_worker::WorkerId;
use std::net::TcpListener;

/// The two channels linking the VNC worker to a synth video device. Both are
/// present when a synth video device exists, absent otherwise.
#[derive(MeshPayload)]
pub struct SynthVideoChannels {
    /// Receives dirty-rectangle hints from the synthetic video device.
    pub dirty_recv: mesh::Receiver<Vec<video_core::DirtyRect>>,
    /// Tells the device whether the guest's screen/pointer updates are
    /// currently needed: `true` when the first client connects, `false` when
    /// the last one disconnects.
    pub updates_needed_send: mesh::Sender<bool>,
}

/// Dirty-tracking tile edge length in pixels. The set is fixed because the
/// bitmap indexing and merge arithmetic assume a power-of-two tile. The command
/// line offers 4, 8, and 16; `Tile2` and `Tile32` exist only for the `Cycle`
/// diagnostic sweep.
#[derive(Copy, Clone, Debug, PartialEq, Eq, MeshPayload)]
pub enum VncTileSize {
    /// 2x2-pixel tiles. Diagnostic sweep only.
    Tile2,
    /// 4x4-pixel tiles.
    Tile4,
    /// 8x8-pixel tiles. The default.
    Tile8,
    /// 16x16-pixel tiles.
    Tile16,
    /// 32x32-pixel tiles. Diagnostic sweep only.
    Tile32,
    /// Measurement mode: cycle through 2, 4, 8, 16, 32 every 30 seconds, logging
    /// the bytes sent at each size to measure the size's impact in a live
    /// session.
    Cycle,
}

impl VncTileSize {
    /// The tile edge length in pixels used by the dirty bitmap. `Cycle` is
    /// resolved by the server, not here; its arm returns 16 for completeness.
    pub fn pixels(self) -> u16 {
        match self {
            Self::Tile2 => 2,
            Self::Tile4 => 4,
            Self::Tile8 => 8,
            Self::Tile16 => 16,
            Self::Tile32 => 32,
            Self::Cycle => 16,
        }
    }
}

/// The VNC server's input parameters.
#[derive(MeshPayload)]
pub struct VncParameters<T> {
    /// The socket the VNC server will listen on
    pub listener: T,
    /// The framebuffer memory.
    pub framebuffer: framebuffer::FramebufferAccess,
    /// A channel to send input to.
    pub input_send: mesh::Sender<input_core::InputData>,
    /// Channels to the synthetic video device, or `None` when no synth video
    /// device is present (e.g. `--pcat` or any guest using a non-synth
    /// framebuffer path like VGA BIOS / UEFI GOP); the server still has a
    /// framebuffer to display and falls back to whole-framebuffer tile-diff
    /// scanning to detect changes.
    pub synth_video: Option<SynthVideoChannels>,
    /// Maximum concurrent VNC clients.
    pub max_clients: usize,
    /// When true, evict the oldest client instead of rejecting new ones.
    pub evict_oldest: bool,
    /// Dirty-tracking tile edge length. `Tile8` is the default.
    pub tile_size: VncTileSize,
}

/// Worker ID for the TCP-listening VNC worker, used by openvmm.
pub const VNC_WORKER_TCP: WorkerId<VncParameters<TcpListener>> = WorkerId::new("VncWorkerTcp");

/// Worker ID for the vmsocket-listening VNC worker, used by openhcl's paravisor.
#[cfg(any(windows, target_os = "linux"))]
pub const VNC_WORKER_VMSOCKET: WorkerId<VncParameters<vmsocket::VmListener>> =
    WorkerId::new("VncWorkerVmSocket");
