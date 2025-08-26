#![expect(dead_code)]
use crate::platform::hypvctx::HvTestCtx;
mod hyperv;

pub fn run_test() {
    let mut ctx = HvTestCtx::new();
    ctx.init(hvdef::Vtl::Vtl0).expect("failed to init on BSP");
    hyperv::hv_tpm_read_cvm::exec(&mut ctx);
}
