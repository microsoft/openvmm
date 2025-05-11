// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Runtime support for the UEFI application environment.

#![cfg(target_os = "uefi")]
// UNSAFETY: Raw assembly needed for panic handling to abort.
use core::arch::asm;

#[panic_handler]
fn panic_handler(panic: &core::panic::PanicInfo<'_>) -> ! {
    log::error!("Panic at runtime: {}", panic);
    unsafe {
        asm!("int 8H");
    }
    loop {}
}
