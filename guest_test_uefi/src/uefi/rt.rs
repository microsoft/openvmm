// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Runtime support for the UEFI application environment.

#![cfg(target_os = "uefi")]
// UNSAFETY: Raw assembly needed for panic handling to abort.
#![expect(unsafe_code)]

#[panic_handler]
fn panic_handler(panic: &core::panic::PanicInfo<'_>) -> ! {
    use uefi::println;

    println!("{}", panic);

    // If the system table is available, use UEFI's standard shutdown mechanism
    if uefi::table::system_table_raw().is_none() {
        use uefi::table::runtime::ResetType;
        uefi::runtime::reset(ResetType::SHUTDOWN, uefi::Status::ABORTED, None);
    }

    println!("Could not shut down... falling back to invoking an undefined instruction");

    // SAFETY: the undefined instruction trap handler in `guest_test_uefi` will not return
    unsafe {
        #[cfg(target_arch = "x86_64")]
        core::arch::asm!("ud2");
        #[cfg(target_arch = "aarch64")]
        core::arch::asm!("brk #0");
        core::hint::unreachable_unchecked();
    }
}
