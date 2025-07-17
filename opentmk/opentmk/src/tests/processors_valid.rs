use hvdef::Vtl;
use sync_nostd::Channel;

use crate::{
    context::{InterruptPlatformTrait, VirtualProcessorPlatformTrait, VpExecutor, VtlPlatformTrait},
    tmk_assert,
};

#[inline(never)]
pub fn exec<T>(ctx: &mut T)
where
    T: VtlPlatformTrait + VirtualProcessorPlatformTrait<T> + InterruptPlatformTrait,
{
     let r = ctx.setup_partition_vtl(Vtl::Vtl1);
    tmk_assert!(r.is_ok(), "setup_partition_vtl should succeed");

    let vp_count = ctx.get_vp_count();
    tmk_assert!(vp_count.is_ok(), "get_vp_count should succeed");

    let vp_count = vp_count.unwrap();
    tmk_assert!(vp_count == 4, "vp count should be 4");

    _ = ctx.setup_interrupt_handler();

    _ = ctx.set_interrupt_idx(0x6, || {
        loop{}
    });

    ctx.start_on_vp(VpExecutor::new(0, Vtl::Vtl1).command(move |ctx: &mut T| {
        log::info!("successfully started running VTL1 on vp0.");
        ctx.switch_to_low_vtl();
    })).expect("Failed to start on VP 0");
}