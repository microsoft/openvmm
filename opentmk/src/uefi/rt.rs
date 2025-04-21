// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Runtime support for the UEFI application environment.

#![cfg(target_os = "uefi")]
// UNSAFETY: Raw assembly needed for panic handling to abort.
#![expect(unsafe_code)]

use crate::arch::serial::{InstrIoAccess, Serial};
use crate::slog;
use crate::sync::Mutex;
use core::arch::asm;
use core::fmt::Write;

#[panic_handler]
fn panic_handler(panic: &core::panic::PanicInfo<'_>) -> ! {
    crate::errorlog!("Panic at runtime: {}", panic);
    unsafe {
        asm!("int 8H");
    }
    loop {}
}
