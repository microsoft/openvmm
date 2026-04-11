// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_os = "uefi")]
#[panic_handler]
fn panic_handler(panic: &core::panic::PanicInfo<'_>) -> ! {
    log::error!("Panic at runtime: {}", panic);
    log::warn!("TEST_END");
    // Best-effort ACPI shutdown on panic; spin if it fails.
    let _ = crate::devices::acpi_shutdown::acpi_shutdown();
    loop {
        core::hint::spin_loop();
    }
}
