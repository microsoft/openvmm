// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Public worker IDs and parameter types for launching the VNC worker.
//!
//! This crate is the shared boundary between the VNC worker process and
//! its launcher (openvmm or openhcl). Two worker variants exist: a TCP
//! listener (used by openvmm) and a vmsocket listener (used by openhcl's
//! paravisor).

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
    /// Channel used to tell the synth video device whether the guest's
    /// screen/pointer updates are currently needed: `true` is sent when the
    /// first client connects, `false` when the last one disconnects. The
    /// device relays this to the guest (via a synthvid `FeatureChange`) so it
    /// stops generating dirty rectangles and pointer reports while no client
    /// is watching. `None` when no synth video device is wired up (the paired
    /// `dirty_recv` is then also `None`).
    pub updates_needed_send: Option<mesh::Sender<bool>>,
}

/// Worker ID for the TCP-listening VNC worker. Used by openvmm, where the
/// server is reachable directly over a host TCP socket.
pub const VNC_WORKER_TCP: WorkerId<VncParameters<TcpListener>> = WorkerId::new("VncWorkerTcp");

/// Worker ID for the vmsocket-listening VNC worker. Used by openhcl's
/// paravisor, where the server is reachable from the host via vsock.
#[cfg(any(windows, target_os = "linux"))]
pub const VNC_WORKER_VMSOCKET: WorkerId<VncParameters<vmsocket::VmListener>> =
    WorkerId::new("VncWorkerVmSocket");
