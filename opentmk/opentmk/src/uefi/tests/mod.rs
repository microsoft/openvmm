#![allow(dead_code)]
use super::hypvctx::HvTestCtx;

mod hv_processor;
mod hv_misc;
mod hv_error_vp_start;


pub fn run_test() {
    let mut ctx = HvTestCtx::new();
    ctx.init().expect("failed to init on BSP");
    hv_processor::exec(&mut ctx);
}