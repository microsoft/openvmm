// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Minimal UEFI application containing OpenVMM bare-metal guest tests.
//!
//! The payload runs before a general-purpose guest OS, allowing tests to issue
//! low-level MMIO and port-I/O operations with little surrounding machinery.
//! Petri selects an action through the `PetriBootAction` UEFI variable.
//!
//! Build this crate for a `*-unknown-uefi` target, then use `cargo xtask
//! guest-test uefi` to place the resulting EFI executable in a bootable disk
//! image. The non-UEFI `main` exists only so workspace-wide host builds work.

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
