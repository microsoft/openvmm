//! This contains the core framework elements for opentmk, which is a simple
//! testing framework that can be compiled into a mini VM image.

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: This crate contains unsafe code to perform low-level operations such as managing memory, handling interrupts, and invoking hypercalls.
#![expect(unsafe_code)]
#![cfg_attr(all(not(test), target_os = "uefi"), no_main)]
#![cfg_attr(all(not(test), target_os = "uefi"), no_std)]
#![allow(missing_docs)]

#[macro_use]
pub extern crate alloc;

pub mod arch;
pub mod context;
pub mod devices;
pub mod platform;
pub mod tmk_assert;
pub mod tmk_logger;
pub mod tmkdefs;
#[cfg(target_os = "uefi")]
pub mod uefi;
