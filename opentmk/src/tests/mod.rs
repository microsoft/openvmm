use crate::platform::hyperv::ctx::HvTestCtx;
pub mod hyperv;

pub fn run_test() {
    let mut ctx = HvTestCtx::new();
    ctx.init(hvdef::Vtl::Vtl0).expect("failed to init on BSP");
    hyperv::hv_processor::exec(&mut ctx);
}
