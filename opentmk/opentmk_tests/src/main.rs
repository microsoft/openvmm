// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Runs a litany of hyper-v tests using opentmk framework

// UNSAFETY: This crate contains unsafe code to perform low-level operations such as managing memory, handling interrupts, and invoking hypercalls.
#![expect(unsafe_code)]
#![cfg_attr(all(not(test), target_os = "uefi"), no_main)]
#![cfg_attr(all(not(test), target_os = "uefi"), no_std)]

#[macro_use]
extern crate alloc;

mod tests;

// Actual entrypoint is `uefi::uefi_main`, via the `#[entry]` macro
#[cfg(any(test, not(target_os = "uefi")))]
fn main() {}

#[cfg(all(not(test), target_os = "uefi"))]
#[uefi::entry]
fn uefi_main() -> uefi::Status {
    let r = opentmk::uefi::init::init();
    opentmk::tmk_assert!(r.is_ok(), "init should succeed");

    log::warn!("TEST_START");
    tests::run_test();
    log::warn!("TEST_END");
    loop {
        core::hint::spin_loop();
    }
}
