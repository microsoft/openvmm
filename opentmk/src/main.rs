// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: This crate contains unsafe code to perform low-level operations such as managing memory, handling interrupts, and invoking hypercalls.
#![expect(unsafe_code)]
#![cfg_attr(nightly, feature(abi_x86_interrupt))]
#![doc = include_str!("../README.md")]
#![cfg_attr(all(not(test), target_os = "uefi"), no_main)]
#![cfg_attr(all(not(test), target_os = "uefi"), no_std)]

// Actual entrypoint is `uefi::uefi_main`, via the `#[entry]` macro
#[cfg(any(test, not(target_os = "uefi")))]
fn main() {}

#[macro_use]
extern crate alloc;

pub mod arch;
pub mod context;
pub mod devices;
pub mod platform;
pub mod tests;
pub mod tmk_assert;
pub mod tmk_logger;
pub mod tmkdefs;
#[cfg(target_os = "uefi")]
mod uefi;
