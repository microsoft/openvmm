// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod alloc;
mod hypvctx;
pub mod init;
mod rt;
mod tests;

use crate::tmk_assert;
use init::init;
use uefi::entry;
use uefi::Status;

#[entry]
fn uefi_main() -> Status {
    let r= init();
    tmk_assert!(r.is_ok(), "init should succeed");

    log::warn!("TEST_START");
    tests::run_test();
    log::warn!("TEST_END");
    Status::SUCCESS
}
