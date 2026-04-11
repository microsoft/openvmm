// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub(crate) mod acpi_wrap;
mod alloc;
pub mod init;
mod rt;

use init::init;
use uefi::Status;

use crate::tmk_assert;

#[uefi::entry]
fn uefi_main() -> Status {
    let r = init();
    tmk_assert!(r.is_ok(), "init should succeed");
    log::warn!("TEST_START");
    crate::tests::run_test();
    log::warn!("TEST_END");
    // Attempt a clean ACPI S5 shutdown; fall back to spinning if it fails.
    let _ = crate::devices::acpi_shutdown::acpi_shutdown();
    loop {
        core::hint::spin_loop();
    }
}
