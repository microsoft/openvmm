// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run virtio devices over different transports

mod mmio;
mod pci;
pub(crate) mod saved_state;
mod task;

pub use mmio::VirtioMmioDevice;
pub use pci::PciInterruptModel;
pub use pci::VirtioPciDevice;
