#![expect(dead_code)]
use crate::platform::hypvctx::HvTestCtx;

mod hv_error_vp_start;
mod hv_misc;
mod hv_processor;
mod hv_cvm_mem_protect;
mod hv_tpm;
pub fn run_test() {
    let mut ctx = HvTestCtx::new();
    ctx.init(hvdef::Vtl::Vtl0).expect("failed to init on BSP");
    hv_tpm::exec(&mut ctx);
}