// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod alloc;
mod context;
pub mod hypercall;
mod hypvctx;
pub mod init;
mod rt;
mod tests;

use crate::tmk_assert::AssertResult;
use init::init;
use uefi::entry;
use uefi::Status;

#[entry]
fn uefi_main() -> Status {
    init().expect_assert("Failed to initialize environment");
    tests::run_test();
    Status::SUCCESS
}
