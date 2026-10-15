// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test modules driving OpenTMK tests.

mod hyperv;

crate::opentmk_backends! {
    hyperv => |_params: &serde_json::Value| {
        let mut ctx = opentmk_core::platform::hyperv::ctx::HvTestCtx::new();
        ctx.init(hvdef::Vtl::Vtl0).expect("failed to init on BSP");
        ctx
    },
}

/// Reads the embedded config and runs the selected backend/test.
/// Panics if the config is invalid or names an unknown backend/test.
pub fn run_test() {
    crate::dispatch::run_test(&crate::config::OPENTMK_CONFIG, dispatch);
}
