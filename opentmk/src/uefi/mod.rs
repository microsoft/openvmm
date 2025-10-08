// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod alloc;
pub mod init;
mod rt;

use init::init;
use uefi::Status;
use uefi::entry;

use crate::tmk_assert;

#[entry]
fn uefi_main() -> Status {
    let r = init();
    tmk_assert!(r.is_ok(), "init should succeed");

    log::warn!("TEST_START");
    crate::tests::run_test();
    log::warn!("TEST_END");
    loop {
        core::hint::spin_loop();
    }
}
