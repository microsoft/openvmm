// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The port and node layer that makes mesh channels process-transparent.
//!
//! A [`local_node::Port`] is a bidirectional, untyped message endpoint. Typed
//! channels (`Sender<T>`, `Receiver<T>`) use ports internally when they need to
//! cross process boundaries. Ports can be sent inside messages, allowing
//! channel endpoints to migrate between processes.
//!
//! Each process has a local node that tracks its ports and connections to
//! remote nodes. The node layer handles routing, port migration, and message
//! ordering.
//!
//! Most code should use the `mesh` facade crate rather than depending on this
//! crate directly.

#![expect(missing_docs)]

pub mod common;
pub mod local_node;
pub mod message;
pub mod resource;
