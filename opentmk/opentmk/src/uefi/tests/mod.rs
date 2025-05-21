use super::hypvctx::HvTestCtx;

pub mod hv_processor;
pub mod hv_misc;
mod hv_error_vp_start;


pub fn run_test() {
    let mut ctx = HvTestCtx::new();
    ctx.init().expect("failed to init on BSP");
    hv_error_vp_start::exec(&mut ctx);
}