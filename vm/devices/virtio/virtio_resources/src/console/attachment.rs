// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Host endpoint attachment and disconnect policy of virtio-console resources.
//!
//! [`VirtioConsoleAttachment`] describes how a console's host endpoint is
//! attached and reconnected, and [`VirtioConsoleDisconnectPolicy`] selects what
//! the device does with guest output while that endpoint is disconnected.

use mesh::MeshPayload;

#[derive(Copy, Clone, Debug, Eq, PartialEq, MeshPayload)]
pub enum VirtioConsoleDisconnectPolicy {
    /// Complete and discard guest transmit descriptors while disconnected.
    Discard,
    /// Retain guest transmit descriptors until the backend reconnects.
    Retain,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, MeshPayload)]
pub enum VirtioConsoleBackendKind {
    UnixSocket,
    NamedPipe,
    Tcp,
    Inherited,
    Disconnected,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, MeshPayload)]
pub enum VirtioConsoleAttachmentMode {
    Listen,
    Connect,
    Inherited,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, MeshPayload)]
pub enum VirtioConsoleReconnectPolicy {
    RecreateListener,
    ReconnectClient,
    RequireInheritedAttachment,
    DiscardWhileDisconnected,
}

#[derive(Clone, Debug, Eq, PartialEq, MeshPayload)]
pub struct VirtioConsoleAttachment {
    pub stable_id: String,
    pub backend_kind: VirtioConsoleBackendKind,
    pub mode: VirtioConsoleAttachmentMode,
    pub endpoint_identity: String,
    pub reconnect_policy: VirtioConsoleReconnectPolicy,
    pub required: bool,
    pub reconnect_timeout_ms: u64,
}
