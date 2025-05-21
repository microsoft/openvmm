use hvdef::Vtl;
use sync_nostd::Channel;

use crate::{
    tmk_assert, context::{TestCtxTrait, VpExecutor}
};

pub fn exec<T>(ctx: &mut T) where T: TestCtxTrait<T> {
    let r = ctx.setup_interrupt_handler();
    tmk_assert!(r.is_ok(), "setup_interrupt_handler should succeed");

    let r = ctx.setup_partition_vtl(Vtl::Vtl1);
    tmk_assert!(r.is_ok(), "setup_partition_vtl should succeed");
    
    let vp_count = ctx.get_vp_count();
    tmk_assert!(vp_count == 8, "vp count should be 8");

    // Testing BSP VTL Bringup
    {
        let (tx, rx) = Channel::new().split();
        let result = ctx.start_on_vp(VpExecutor::new(0, Vtl::Vtl1).command(
            move |ctx: &mut T| {
                let vp = ctx.get_current_vp();
                tmk_assert!(vp.is_ok(), "vp should be valid");
                
                let vp = vp.unwrap();
                log::info!("vp: {}", vp);
                tmk_assert!(vp == 0, "vp should be equal to 0");

                let vtl = ctx.get_current_vtl();
                tmk_assert!(vtl.is_ok(), "vtl should be valid");

                let vtl = vtl.unwrap();
                log::info!("vtl: {:?}", vtl);
                tmk_assert!(vtl == Vtl::Vtl1, "vtl should be Vtl1 for BSP");
                tx.send(()).expect("Failed to send message through the channel");
                ctx.switch_to_low_vtl();
            },
        ));
        tmk_assert!(result.is_ok(), "start_on_vp should succeed");
        _ = rx.recv();
    }

    for i in 1..vp_count {
        // Testing VTL1
        {
            let (tx, rx) = Channel::new().split();
            let result = ctx.start_on_vp(VpExecutor::new(i, Vtl::Vtl1).command(
                move |ctx: &mut T| {
                    let vp = ctx.get_current_vp();
                    tmk_assert!(vp.is_ok(), "vp should be valid");
                    
                    let vp = vp.unwrap();
                    log::info!("vp: {}", vp);
                    tmk_assert!(vp == i, format!("vp should be equal to {}", i));

                    let vtl = ctx.get_current_vtl();
                    tmk_assert!(vtl.is_ok(), "vtl should be valid");

                    let vtl = vtl.unwrap();
                    log::info!("vtl: {:?}", vtl);
                    tmk_assert!(vtl == Vtl::Vtl1, format!("vtl should be Vtl1 for VP {}", i));
                    _ = tx.send(());
                },
            ));
            tmk_assert!(result.is_ok(), "start_on_vp should succeed");
            _ = rx.recv();
        }

        // Testing VTL0
        {
            let (tx, rx) = Channel::new().split();
            let result = ctx.start_on_vp(VpExecutor::new(i, Vtl::Vtl0).command(
                move |ctx: &mut T| {
                    let vp = ctx.get_current_vp();
                    tmk_assert!(vp.is_ok(), "vp should be valid");

                    let vp = vp.unwrap();
                    log::info!("vp: {}", vp);
                    tmk_assert!(vp == i, format!("vp should be equal to {}", i));

                    let vtl = ctx.get_current_vtl();
                    tmk_assert!(vtl.is_ok(), "vtl should be valid");

                    let vtl = vtl.unwrap();
                    log::info!("vtl: {:?}", vtl);
                    tmk_assert!(vtl == Vtl::Vtl0, format!("vtl should be Vtl0 for VP {}", i));
                    _ = tx.send(());
                },
            ));
            tmk_assert!(result.is_ok(), "start_on_vp should succeed");
            _ = rx.recv();
        }
    }

    log::warn!("All VPs have been tested");
}
