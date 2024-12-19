// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

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
