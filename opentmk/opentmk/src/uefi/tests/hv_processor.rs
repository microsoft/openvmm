use hvdef::Vtl;
use sync_nostd::Channel;

use crate::{
    tmk_assert, uefi::context::{TestCtxTrait, VpExecutor}
};

pub fn exec(ctx: &mut dyn TestCtxTrait) {
    ctx.setup_interrupt_handler();
    ctx.setup_partition_vtl(Vtl::Vtl1);

    let vp_count = ctx.get_vp_count();
    tmk_assert!(vp_count == 8, "vp count should be 8");

    // Testing BSP VTL Bringup
    {
        let (tx, rx) = Channel::new().split();
        ctx.start_on_vp(VpExecutor::new(0, Vtl::Vtl1).command(
            move |ctx: &mut dyn TestCtxTrait| {
                let vp = ctx.get_current_vp();
                log::info!("vp: {}", vp);
                tmk_assert!(vp == 0, "vp should be equal to 0");

                let vtl = ctx.get_current_vtl();
                log::info!("vtl: {:?}", vtl);
                tmk_assert!(vtl == Vtl::Vtl1, "vtl should be Vtl1 for BSP");
                _ = tx.send(());
                ctx.switch_to_low_vtl();
            },
        ));
        _ = rx.recv();
    }

    for i in 1..vp_count {
        // Testing VTL1
        {
            let (tx, rx) = Channel::new().split();
            ctx.start_on_vp(VpExecutor::new(i, Vtl::Vtl1).command(
                move |ctx: &mut dyn TestCtxTrait| {
                    let vp = ctx.get_current_vp();
                    log::info!("vp: {}", vp);
                    tmk_assert!(vp == i, format!("vp should be equal to {}", i));

                    let vtl = ctx.get_current_vtl();
                    log::info!("vtl: {:?}", vtl);
                    tmk_assert!(vtl == Vtl::Vtl1, format!("vtl should be Vtl1 for VP {}", i));
                    _ = tx.send(());
                },
            ));
            _ = rx.recv();
        }

        // Testing VTL0
        {
            let (tx, rx) = Channel::new().split();
            ctx.start_on_vp(VpExecutor::new(i, Vtl::Vtl0).command(
                move |ctx: &mut dyn TestCtxTrait| {
                    let vp = ctx.get_current_vp();
                    log::info!("vp: {}", vp);
                    tmk_assert!(vp == i, format!("vp should be equal to {}", i));

                    let vtl = ctx.get_current_vtl();
                    log::info!("vtl: {:?}", vtl);
                    tmk_assert!(vtl == Vtl::Vtl0, format!("vtl should be Vtl0 for VP {}", i));
                    _ = tx.send(());
                },
            ));
            _ = rx.recv();
        }
    }

    log::warn!("All VPs have been tested");
}
