// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core virtio device infrastructure

#![expect(missing_docs)]

mod common;
pub mod device;
pub mod queue;
pub mod regions;
pub mod resolve;
pub mod resolver;
pub mod test_helpers;
mod tests;
pub mod transport;

pub use common::*;
pub use device::DynVirtioDevice;
pub use device::VirtioDevice;
pub use transport::*;
pub use virtio_spec as spec;

pub const QUEUE_MAX_SIZE: u16 = 0x40; // TODO: make queue size configurable
