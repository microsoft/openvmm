// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! gRPC-style client and server implementation.
//!
//! This provides [gRPC](https://grpc.io/) and
//! [ttrpc](https://github.com/containerd/ttrpc) servers and clients that
//! interop well with mesh channels, allowing gRPC to be easily used with a
//! mesh-based application.
//!
//! Currently, the server supports the gRPC and ttrpc protocols, while the
//! client only supports the ttrpc protocol.

#![warn(missing_docs)]

#[cfg(test)]
extern crate self as mesh_rpc;

mod client;
mod message;
mod rpc;
pub mod server;
pub mod service;

pub use client::Client;
pub use server::Server;
