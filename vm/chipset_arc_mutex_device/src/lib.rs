// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VMM-agnostic infrastructure to wire up
//! [`ChipsetDevice`](chipset_device::ChipsetDevice) instances using
//! `Arc<CloseableMutex<dyn ChipsetDevice>>` to communicate with the backing chipset.
//!
//! NOTE: this crate is no longer used by OpenVMM/OpenHCL, and only remains
//! in-tree to support testing devices.


pub mod device;
pub mod services;

pub mod test_chipset;
