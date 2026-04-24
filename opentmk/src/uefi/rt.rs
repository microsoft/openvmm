// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_os = "uefi")]
#[panic_handler]
fn panic_handler(panic: &core::panic::PanicInfo<'_>) -> ! {
    log::error!("Panic at runtime: {}", panic);
    log::warn!("TEST_END");
    // Best-effort ACPI shutdown on panic. On UEFI this never returns --
    // it either powers off the VM or spins forever internally.
    let _ = crate::devices::shutdown::shutdown();
    // Unreachable on UEFI, but required for the `-> !` return type.
    loop {
        core::hint::spin_loop();
    }
}
