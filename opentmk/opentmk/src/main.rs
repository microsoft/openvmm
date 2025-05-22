// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
#![no_std]
#![allow(unsafe_code)]
#![feature(abi_x86_interrupt)]
#![feature(naked_functions)]

#![doc = include_str!("../README.md")]

#![cfg_attr(all(not(test), target_os = "uefi"), no_main)]
#![cfg_attr(all(not(test), target_os = "uefi"), no_std)]

// Actual entrypoint is `uefi::uefi_main`, via the `#[entry]` macro
#[cfg(any(test, not(target_os = "uefi")))]
fn main() {}

#[macro_use]
extern crate alloc;

mod uefi;
pub mod arch;
pub mod tmk_assert;
pub mod tmk_logger;
pub mod hypercall;
pub mod context;
pub mod tmkdefs;