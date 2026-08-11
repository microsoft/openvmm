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

#![cfg_attr(not(test), no_std)]

extern crate alloc;
