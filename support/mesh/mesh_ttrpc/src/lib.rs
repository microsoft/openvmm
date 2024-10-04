// Copyright (C) Microsoft Corporation. All rights reserved.

//! gRPC-style client and server implementation.
//!
//! TODO: rename this crate
//!
//! Currently, the server supports the gRPC and ttrpc protocols, while the
//! client only supports the ttrpc protocol.
//!
//! ttrpc is a low-overhead, high-density local RPC interface used for
//! containerd to communicate with its shims and plugins. It uses the same
//! payload format as gRPC but a much simpler transport format.

#![warn(missing_docs)]

#[cfg(test)]
extern crate self as mesh_ttrpc;

mod client;
mod message;
mod rpc;
mod server;
pub mod service;

pub use client::Client;
pub use server::Server;
