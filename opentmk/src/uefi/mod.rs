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
    // Attempt a clean ACPI S5 shutdown. On UEFI this never returns --
    // it either powers off the VM or spins forever internally.
    // The loop below is unreachable but satisfies the Status return type.
    let _ = crate::devices::shutdown::shutdown();
    loop {
        core::hint::spin_loop();
    }
}
