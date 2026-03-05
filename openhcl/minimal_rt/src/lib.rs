// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Minimal support runtime for `openhcl_boot` and other kernel-mode
//! environments.

#![no_std]
// UNSAFETY: Interacting with low level hardware and bootloader primitives.
#![expect(unsafe_code)]

pub mod arch;
pub mod enlightened_panic;
pub mod reftime;
pub mod rt;

pub use minimal_rt_reloc as reloc;
