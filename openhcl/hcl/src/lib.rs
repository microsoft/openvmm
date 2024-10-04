// Copyright (C) Microsoft Corporation. All rights reserved.

//! Crate for interacting with the hypervisor via the /dev/mshv_vtl device and
//! related kernel functionality.

#![cfg(target_os = "linux")]
#![warn(missing_docs)]
// UNSAFETY: Calling ioctls.
#![allow(unsafe_code)]

pub mod ioctl;
pub mod protocol;
pub mod stats;
pub mod vmbus;
pub mod vmsa;
