mod hv_processor;
mod hv_misc;

use crate::uefi::hypvctx::HvTestCtx;

pub fn run_test() {
    let mut ctx = HvTestCtx::new();
    hv_processor::exec(&mut ctx);
}