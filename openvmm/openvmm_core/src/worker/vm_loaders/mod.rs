// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// TODO: move to a separate crate.

mod device_tree;
#[cfg(guest_arch = "aarch64")]
pub(crate) use device_tree::FixedGicInterrupt;
#[cfg(guest_arch = "aarch64")]
pub(crate) use device_tree::FixedMmioDevice;
pub mod igvm;
pub mod linux;
pub mod microvm;
pub mod pcat;
pub mod uefi;
