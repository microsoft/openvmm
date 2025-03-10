// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
#![allow(warnings)]
#![no_std]
#![allow(unsafe_code)]
#![feature(naked_functions)]
#![feature(abi_x86_interrupt)]
#![feature(concat_idents)]

#![doc = include_str!("../README.md")]
// HACK: workaround for building guest_test_uefi as part of the workspace in CI.
#![cfg_attr(all(not(test), target_os = "uefi"), no_main)]
#![cfg_attr(all(not(test), target_os = "uefi"), no_std)]

// HACK: workaround for building guest_test_uefi as part of the workspace in CI
//
// Actual entrypoint is `uefi::uefi_main`, via the `#[entry]` macro
#[cfg(any(test, not(target_os = "uefi")))]
fn main() {}

#[macro_use]
extern crate alloc;

mod uefi;
pub mod arch;
pub mod slog;
pub mod sync;