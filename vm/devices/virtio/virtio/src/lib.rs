// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core virtio device infrastructure

mod common;
pub mod queue;
pub mod resolve;
pub mod spec;
pub mod transport;

pub use common::*;
pub use transport::*;

const QUEUE_MAX_SIZE: u16 = 0x40; // TODO: make queue size configurable
