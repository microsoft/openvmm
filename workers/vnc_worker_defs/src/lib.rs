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
    /// Receives dirty rectangles from the synthetic video device.
    /// None when the video device is not configured.
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
