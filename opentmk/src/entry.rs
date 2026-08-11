// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! UEFI entrypoint for the OpenTMK test binary.

use opentmk_core::tmk_assert;
use opentmk_core::uefi::init::init;
use uefi::Status;

#[uefi::entry]
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
