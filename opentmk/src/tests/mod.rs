// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test modules driving OpenTMK tests.

// only one test is run at a time so there is dead code in other tests
#![expect(dead_code)]
use crate::platform::hyperv::ctx::HvTestCtx;
mod hyperv;

/// Runs all the tests.
pub fn run_test() {
    let mut ctx = HvTestCtx::new();
    ctx.init(hvdef::Vtl::Vtl0).expect("failed to init on BSP");
    hyperv::hv_processor::exec(&mut ctx);
}
