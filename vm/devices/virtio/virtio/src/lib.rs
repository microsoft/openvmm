// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core virtio device infrastructure

#![expect(missing_docs)]

mod common;
pub mod queue;
pub mod resolve;
pub mod resolver;
pub mod spec;
mod tests;
pub mod transport;

pub use common::*;
pub use transport::*;

const QUEUE_MAX_SIZE: u16 = 0x40; // TODO: make queue size configurable
