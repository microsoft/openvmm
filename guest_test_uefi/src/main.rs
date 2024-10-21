// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: lots of low-level direct access to hardware and usage of inherently
// unsafe UEFI APIs.
//
// On the bright side - this is entirely test code, and is not used anything
// near production!
#![allow(unsafe_code)]
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
