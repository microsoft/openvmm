// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[panic_handler]
fn panic_handler(panic: &core::panic::PanicInfo<'_>) -> ! {
    log::error!("Panic at runtime: {}", panic);
    loop {}
}
