#![expect(dead_code)]
use crate::platform::hypvctx::HvTestCtx;

mod hv_error_vp_start;
mod hv_misc;
mod hv_processor;
mod tpm_test;

pub fn run_test() {
    let mut ctx = HvTestCtx::new();
    ctx.init().expect("failed to init on BSP");
    hv_misc::exec(&mut ctx);
}