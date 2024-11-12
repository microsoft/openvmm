// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mesh RPC node implementations for cross-process.

mod alpc_node;
mod common;
mod point_to_point;
mod protocol;
mod unix_node;

#[cfg(windows)]
pub mod windows {
    //! Windows-specific mesh functionality.

    use super::alpc_node;
    pub use alpc_node::AlpcNode;
    pub use alpc_node::Invitation as AlpcInvitation;
    pub use alpc_node::InvitationHandle as AlpcInvitationHandle;
}

#[cfg(unix)]
pub mod unix {
    //! Unix-specific mesh functionality.

    use super::unix_node;
    pub use unix_node::Invitation as UnixInvitation;
    pub use unix_node::UnixNode;
}

pub use common::InvitationAddress;
pub use point_to_point::PointToPointMesh;
