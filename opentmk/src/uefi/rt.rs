// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_os = "uefi")]
#[panic_handler]
fn panic_handler(panic: &core::panic::PanicInfo<'_>) -> ! {
    log::error!("Panic at runtime: {}", panic);
    log::warn!("TEST_END");
    loop {}
}
