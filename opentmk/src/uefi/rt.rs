// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Runtime support for the UEFI application environment.

#![cfg(target_os = "uefi")]
// UNSAFETY: Raw assembly needed for panic handling to abort.
#![expect(unsafe_code)]

use crate::arch::serial::{Serial, InstrIoAccess};
use core::fmt::Write;
use crate::slog;
use crate::sync::Mutex;

#[panic_handler]
fn panic_handler(panic: &core::panic::PanicInfo<'_>) -> ! {

    let io = InstrIoAccess {};
    let mut ser = Mutex::new(Serial::new(io));
    crate::errorlog!("Panic at runtime: {}", panic);
    crate::errorlog!("Could not shut down... falling back to invoking an undefined instruction");
    loop{}
}
