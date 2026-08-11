// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core library for OpenTMK, the UEFI test harness.
//!
//! This crate holds everything reusable by a test binary: architecture
//! support, the platform abstraction traits and their hypervisor backends,
//! device drivers, the serial JSON logger, assertions, and the UEFI runtime
//! (allocator, ACPI wrapper, init).
//!
//! The consuming binary supplies the test scenarios plus the pieces that must
//! live in the final `.efi` artifact: the `#[uefi::entry]` entrypoint, the
//! panic handler, and the build-time-patchable config region. It drives a run
//! via [`run_test`].

// UNSAFETY: This crate contains unsafe code to perform low-level operations such as managing memory, handling interrupts, and invoking hypercalls.
#![expect(unsafe_code)]
#![cfg_attr(not(test), no_std)]

#[macro_use]
extern crate alloc;

pub mod arch;
pub mod context;
pub mod devices;
pub mod dispatch;
pub mod platform;
pub mod test_helpers;
pub mod tmk_assert;
pub mod tmk_logger;
pub mod tmkdefs;
#[cfg(target_os = "uefi")]
pub mod uefi;

pub use dispatch::run_test;
