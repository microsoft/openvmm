// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod alloc;
mod context;
pub mod hypercall;
mod hypvctx;
pub mod init;
mod rt;
mod tests;

use crate::slog::{AssertOption, AssertResult};
use crate::sync::{Channel, Receiver, Sender};
use crate::uefi::alloc::ALLOCATOR;
use crate::{infolog, tmk_assert};
use ::alloc::boxed::Box;
use ::alloc::vec::Vec;
use alloc::SIZE_1MB;
use context::{TestCtxTrait, VpExecutor};
use core::alloc::{GlobalAlloc, Layout};
use core::cell::RefCell;
use core::ops::Range;
use core::sync::atomic::{AtomicI32, Ordering};
use hvdef::hypercall::HvInputVtl;
use hvdef::Vtl;
use hypvctx::HvTestCtx;
use init::init;
use uefi::entry;
use uefi::Status;

#[entry]
fn uefi_main() -> Status {
    init().expect_assert("Failed to initialize environment");
    tests::run_test();
    Status::SUCCESS
}
