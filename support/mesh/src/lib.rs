// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Channel-based communication framework for OpenVMM and OpenHCL.
//!
//! mesh provides typed channels (`Sender<T>` / `Receiver<T>`) that work
//! the same way whether the two ends are in the same process or in
//! different ones. Components that communicate through mesh channels can
//! be moved between processes without changing their code.
//!
//! This is the facade crate — it re-exports the important types from the
//! lower-level `mesh_channel`, `mesh_node`, `mesh_protobuf`, and
//! `mesh_derive` crates. Most code should only depend on this crate.
//!
//! # Channels
//!
//! - [`channel()`] — multi-producer channel (`Sender<T>` / `Receiver<T>`).
//! - [`oneshot()`] — single-use transfer (`OneshotSender<T>` /
//!   `OneshotReceiver<T>`).
//! - [`rpc::Rpc`] — request/response: bundles a request with a reply
//!   channel. The conventional pattern is an enum of `Rpc` variants.
//! - [`Cell`] / [`CellUpdater`] — publish-subscribe for
//!   configuration updates.
//! - [`pipe`] — `AsyncRead` / `AsyncWrite` byte stream with backpressure.
//! - [`CancelContext`] — cooperative cancellation with deadlines.
//!
//! # Message types
//!
//! Any type can be sent over a channel within a single process. To cross
//! process boundaries, derive [`MeshPayload`]:
//!
//! ```rust,ignore
//! use mesh::rpc::Rpc;
//!
//! #[derive(MeshPayload)]
//! enum MyRequest {
//!     DoThing(Rpc<Input, Output>),
//!     Stop(Rpc<(), ()>),
//! }
//! ```
//!
//! `MeshPayload` types can include channel endpoints, file descriptors,
//! and OS handles as fields — these are transferred automatically when
//! the message crosses a process boundary.
//!
//! See the [mesh guide](https://openvmm.dev/guide/reference/architecture/openvmm/mesh.html)
//! for a full introduction.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

#[expect(unused_extern_crates)]
extern crate self as mesh;

pub mod payload {
    pub use mesh_derive::MeshProtobuf as Protobuf;
    pub use mesh_protobuf::*;
}

pub use mesh_channel::ChannelError;
pub use mesh_channel::ChannelErrorKind;
pub use mesh_channel::OneshotReceiver;
pub use mesh_channel::OneshotSender;
pub use mesh_channel::Receiver;
pub use mesh_channel::RecvError;
pub use mesh_channel::Sender;
pub use mesh_channel::TryRecvError;
pub use mesh_channel::cancel::Cancel;
pub use mesh_channel::cancel::CancelContext;
pub use mesh_channel::cancel::CancelReason;
pub use mesh_channel::cancel::Cancelled;
pub use mesh_channel::cancel::Deadline;
pub use mesh_channel::cell::Cell;
pub use mesh_channel::cell::CellUpdater;
pub use mesh_channel::cell::cell;
pub use mesh_channel::channel;
pub use mesh_channel::error;
pub use mesh_channel::mpsc_channel;
pub use mesh_channel::oneshot;
pub use mesh_channel::pipe;
pub use mesh_channel::rpc;
pub use mesh_derive::MeshPayload;
pub use mesh_node::common::Address;
pub use mesh_node::common::NodeId;
pub use mesh_node::common::PortId;
pub use mesh_node::common::Uuid;
pub use mesh_node::local_node;
pub use mesh_node::message;
pub use mesh_node::message::MeshPayload;
pub use mesh_node::message::Message;
pub use mesh_node::message::OwnedMessage;
pub use mesh_node::resource;
