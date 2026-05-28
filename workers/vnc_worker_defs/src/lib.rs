// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

use mesh::MeshPayload;
use mesh_worker::WorkerId;
use std::net::TcpListener;

/// The VNC server's input parameters.
#[derive(MeshPayload)]
pub struct VncParameters<T> {
    /// The socket the VNC server will listen on
    pub listener: T,
    /// The framebuffer memory.
    pub framebuffer: framebuffer::FramebufferAccess,
    /// A channel to send input to.
    pub input_send: mesh::Sender<input_core::InputData>,
    /// Receives dirty-rectangle hints from the synthetic video device.
    /// `None` when no synth video device is present (e.g. `--pcat` or any
    /// guest using a non-synth framebuffer path like VGA BIOS / UEFI GOP);
    /// the server still has a framebuffer to display and falls back to
    /// whole-framebuffer tile-diff scanning to detect changes.
    pub dirty_recv: Option<mesh::Receiver<Vec<video_core::DirtyRect>>>,
    /// Maximum concurrent VNC clients.
    pub max_clients: usize,
    /// When true, evict the oldest client instead of rejecting new ones.
    pub evict_oldest: bool,
}

pub const VNC_WORKER_TCP: WorkerId<VncParameters<TcpListener>> = WorkerId::new("VncWorkerTcp");

#[cfg(any(windows, target_os = "linux"))]
pub const VNC_WORKER_VMSOCKET: WorkerId<VncParameters<vmsocket::VmListener>> =
    WorkerId::new("VncWorkerVmSocket");
