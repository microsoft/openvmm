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
//!
//! # Usage
//!
//! 1. Define your service in a `.proto` file.
//! 2. Use `mesh_build::MeshServiceGenerator` in your `build.rs` to generate
//!    Rust traits and types for the service.
//! 3. Implement the generated service trait.
//! 4. Create a [`Server`] and register your service implementation.
//!
//! See `mesh_rpc/examples/rust-server.rs` for a working example.

#![forbid(unsafe_code)]

#[cfg(test)]
extern crate self as mesh_rpc;

pub mod client;
mod message;
mod rpc;
pub mod server;
pub mod service;

pub use client::Client;
pub use server::Server;
