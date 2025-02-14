// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

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
}

pub const VNC_WORKER_TCP: WorkerId<VncParameters<TcpListener>> = WorkerId::new("VncWorkerTcp");

#[cfg(any(windows, target_os = "linux"))]
pub const VNC_WORKER_VMSOCKET: WorkerId<VncParameters<vmsocket::VmListener>> =
    WorkerId::new("VncWorkerVmSocket");
