use alloc::sync::Arc;

use super::hypvctx::HvTestCtx;

pub mod hv_processor;
pub mod hv_misc;

pub fn run_test() {
    let mut ctx = HvTestCtx::new();
    ctx.init();
    hv_processor::exec(&mut ctx);
}