// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: This crate defines the config region patched by host tooling, and its tests use inline assembly.
#![expect(unsafe_code)]
#![doc = include_str!("../README.md")]
#![cfg_attr(all(not(test), target_os = "uefi"), no_main)]
#![cfg_attr(all(not(test), target_os = "uefi"), no_std)]

// Actual entrypoint is `entry::uefi_main`, via the `#[entry]` macro
#[cfg(any(test, not(target_os = "uefi")))]
fn main() {}

#[macro_use]
extern crate alloc;

pub mod config;
pub mod dispatch;
#[cfg(target_os = "uefi")]
mod entry;
#[cfg(target_os = "uefi")]
mod rt;
pub mod test_helpers;
pub mod tests;
pub mod tmk_assert;
