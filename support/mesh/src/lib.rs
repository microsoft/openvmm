// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mesh IPC implementation based on channels.
//!
//! This crate provides cross-process message-based communication over channels.

#[expect(unused_extern_crates)]
extern crate self as mesh;

pub mod payload {
    pub use mesh_derive::MeshProtobuf as Protobuf;
    pub use mesh_protobuf::*;
}

pub use mesh_channel::cancel::Cancel;
pub use mesh_channel::cancel::CancelContext;
pub use mesh_channel::cancel::CancelReason;
pub use mesh_channel::cancel::Cancelled;
pub use mesh_channel::cell::cell;
pub use mesh_channel::cell::Cell;
pub use mesh_channel::cell::CellUpdater;
pub use mesh_channel::channel;
pub use mesh_channel::error;
pub use mesh_channel::mpsc_channel;
pub use mesh_channel::oneshot;
pub use mesh_channel::pipe;
pub use mesh_channel::rpc;
pub use mesh_channel::ChannelError;
pub use mesh_channel::ChannelErrorKind;
pub use mesh_channel::MpscReceiver;
pub use mesh_channel::MpscSender;
pub use mesh_channel::OneshotReceiver;
pub use mesh_channel::OneshotSender;
pub use mesh_channel::Receiver;
pub use mesh_channel::RecvError;
pub use mesh_channel::Sender;
pub use mesh_channel::TryRecvError;
pub use mesh_derive::MeshPayload;
pub use mesh_node::common::Address;
pub use mesh_node::common::NodeId;
pub use mesh_node::common::PortId;
pub use mesh_node::common::Uuid;
pub use mesh_node::local_node;
pub use mesh_node::message;
pub use mesh_node::message::MeshPayload;
pub use mesh_node::message::Message;
pub use mesh_node::resource;
pub use mesh_node::upcast;
