// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
#![no_std]
#![allow(unsafe_code)]
#![feature(abi_x86_interrupt)]

#![doc = include_str!("../README.md")]

// Actual entrypoint is `uefi::uefi_main`, via the `#[entry]` macro
#[cfg(any(test, not(target_os = "uefi")))]
fn main() {}

#[macro_use]
extern crate alloc;

mod uefi;
pub mod arch;
pub mod tmk_assert;
pub mod tmk_logger;
