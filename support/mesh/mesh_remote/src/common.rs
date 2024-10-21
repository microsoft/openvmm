// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common functionality for nodes.

use mesh_node::common::Address;
use mesh_protobuf::Protobuf;

/// The protobuf-serializable portion of an invitation.
///
/// This will be combined with a node-type-specific resource to form a full
/// invitation.
#[derive(Debug, Clone, Protobuf)]
pub struct InvitationAddress {
    /// The local address of the port.
    pub local_addr: Address,
    /// The remote address of the port.
    pub remote_addr: Address,
}
