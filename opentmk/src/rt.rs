// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_os = "uefi")]
#[panic_handler]
fn panic_handler(panic: &core::panic::PanicInfo<'_>) -> ! {
    log::error!("{}: {}", opentmk_protocol::PANIC_PREFIX, panic);
    log::warn!("{}", opentmk_protocol::TEST_END);
    loop {}
}
