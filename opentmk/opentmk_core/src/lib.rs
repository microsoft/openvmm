// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core library for OpenTMK, the UEFI test harness.
//!
//! This crate holds the platform-facing half of OpenTMK: architecture support,
//! the platform abstraction traits and their hypervisor backends, device
//! drivers, the serial JSON logger, and the UEFI runtime (allocator, ACPI
//! wrapper, init).
//!
//! The consuming test binary owns everything test-specific: the scenarios, the
//! registration and dispatch macros, assertions, the `#[uefi::entry]`
//! entrypoint, the panic handler, and the build-time-patchable config region.

// UNSAFETY: This crate contains unsafe code to perform low-level operations such as managing memory, handling interrupts, and invoking hypercalls.
#![expect(unsafe_code)]
#![cfg_attr(not(test), no_std)]

#[macro_use]
extern crate alloc;

pub mod arch;
pub mod context;
pub mod devices;
pub mod platform;
pub mod tmk_logger;
pub mod tmkdefs;
#[cfg(target_os = "uefi")]
pub mod uefi;
