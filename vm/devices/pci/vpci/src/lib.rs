// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtual PCI bus emulator, providing a PCI bus over a vmbus transport.

#![forbid(unsafe_code)]

pub mod bus;
pub mod bus_control;
mod device;
mod protocol;
mod test_helpers;
