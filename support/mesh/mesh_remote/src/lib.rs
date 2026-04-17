// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cross-process mesh transport implementations.
//!
//! This crate provides the platform-specific IPC transports that allow mesh
//! ports to communicate across process boundaries:
//!
//! - **Unix** (`UnixNode`) — uses Unix domain sockets with
//!   `SCM_RIGHTS` for file descriptor passing. Child processes
//!   authenticate via a pre-connected socket FD inherited at spawn.
//!   External processes join via a filesystem socket path, where
//!   security depends on directory permissions.
//! - **Windows** (`AlpcNode`) — uses ALPC (Advanced Local Procedure
//!   Call) with handle duplication. Both child and external processes
//!   authenticate using a 256-bit random `MeshSecret`, validated with
//!   constant-time comparison.
//!
//! Most code does not interact with this crate directly. Instead, use
//! `mesh_process::Mesh` to create and manage process groups.

mod alpc_listener;
mod alpc_node;
mod common;
mod point_to_point;
mod protocol;
mod test_common;
mod unix_common;
mod unix_listener;
mod unix_node;

#[cfg(windows)]
pub mod windows {
    //! Windows-specific mesh functionality.

    use super::alpc_listener;
    use super::alpc_node;
    pub use alpc_listener::AlpcMeshListener;
    pub use alpc_listener::FinishError as AlpcFinishError;
    pub use alpc_listener::JoinBySocketError as AlpcJoinBySocketError;
    pub use alpc_listener::PendingMeshConnection as AlpcPendingMeshConnection;
    pub use alpc_node::AlpcNode;
    pub use alpc_node::Invitation as AlpcInvitation;
    pub use alpc_node::InvitationCredentials as AlpcInvitationCredentials;
    pub use alpc_node::InvitationHandle as AlpcInvitationHandle;
    pub use alpc_node::InviteError as AlpcInviteError;
    pub use alpc_node::JoinError as AlpcJoinError;
    pub use alpc_node::NamedInvitation as AlpcNamedInvitation;
    pub use alpc_node::NewNodeError as AlpcNewNodeError;
}

#[cfg(unix)]
pub mod unix {
    //! Unix-specific mesh functionality.

    use super::unix_listener;
    use super::unix_node;
    pub use unix_listener::AcceptError;
    pub use unix_listener::HandshakeError;
    pub use unix_listener::JoinByPathError;
    pub use unix_listener::ListenError;
    pub use unix_listener::PendingMeshConnection;
    pub use unix_listener::UnixMeshListener;
    pub use unix_node::Invitation as UnixInvitation;
    pub use unix_node::InviteError;
    pub use unix_node::JoinError;
    pub use unix_node::UnixNode;
}

pub use common::InvitationAddress;
pub use point_to_point::PointToPointMesh;
