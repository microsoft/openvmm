// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core virtio device infrastructure

#![cfg_attr(not(test), forbid(unsafe_code))]
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

/// Default queue size for virtio devices. Devices that don't override
/// [`VirtioDevice::queue_size`] will use this value.
pub const DEFAULT_QUEUE_SIZE: u16 = 256;

/// Maximum queue size the transport will accept from the guest.
pub const MAX_QUEUE_SIZE: u16 = 1024;
