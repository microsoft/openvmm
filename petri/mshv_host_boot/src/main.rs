// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fixed-sequence, development-only x64 mshv UEFI host boot application.

#![cfg_attr(target_os = "uefi", no_main)]
#![cfg_attr(target_os = "uefi", no_std)]

#[cfg(target_os = "uefi")]
extern crate alloc;

#[cfg(target_os = "uefi")]
mod uefi;

#[cfg(not(target_os = "uefi"))]
fn main() {}
