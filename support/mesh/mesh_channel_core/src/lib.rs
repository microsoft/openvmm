// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core mesh channel primitives: [`Sender`] / [`Receiver`] (mpsc) and
//! [`OneshotSender`] / [`OneshotReceiver`] (single-use).
//!
//! These are the fundamental typed wrappers around the binary [`Port`](mesh_node::local_node::Port)
//! layer. `Sender<T>` serializes a `T` into a port message on send;
//! `Receiver<T>` deserializes it on receive.
//!
//! The `mesh_channel` crate adds higher-level abstractions (RPC, Cell, Cancel,
//! Pipe) on top of these. Most code should use the `mesh` facade crate rather
//! than depending on this crate directly.

mod deque;
mod error;
mod mpsc;
mod oneshot;
mod sync_unsafe_cell;

pub use error::*;
pub use mpsc::*;
pub use oneshot::*;
