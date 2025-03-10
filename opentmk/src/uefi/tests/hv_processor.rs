use alloc::vec::Vec;
use hvdef::Vtl;

use crate::{
    criticallog, infolog, sync::{self, Mutex}, tmk_assert, uefi::context::{TestCtxTrait, VpExecutor}
};

static VP_RUNNING: Mutex<Vec<(u32, Vtl)>> = Mutex::new(Vec::new());

pub fn exec(ctx: &mut dyn TestCtxTrait) {
    ctx.setup_interrupt_handler();
    ctx.setup_partition_vtl(Vtl::Vtl1);

    let vp_count = ctx.get_vp_count();
    tmk_assert!(vp_count == 8, "vp count should be 8");

    // Testing BSP VTL Bringup
    {
        let (mut tx, mut rx) = crate::sync::Channel::new().split();
        ctx.start_on_vp(VpExecutor::new(0, Vtl::Vtl1).command(
            move |ctx: &mut dyn TestCtxTrait| {
                let vp = ctx.get_current_vp();
                infolog!("vp: {}", vp);
                tmk_assert!(vp == 0, "vp should be equal to 0");

                let vtl = ctx.get_current_vtl();
                infolog!("vtl: {:?}", vtl);
                tmk_assert!(vtl == Vtl::Vtl1, "vtl should be Vtl1 for BSP");
                tx.send(());
                ctx.switch_to_low_vtl();
            },
        ));
        rx.recv();
    }

    for i in 1..vp_count {
        // Testing VTL1
        {
            let (mut tx, mut rx) = crate::sync::Channel::new().split();
            ctx.start_on_vp(VpExecutor::new(i, Vtl::Vtl1).command(
                move |ctx: &mut dyn TestCtxTrait| {
                    let vp = ctx.get_current_vp();
                    infolog!("vp: {}", vp);
                    tmk_assert!(vp == i, format!("vp should be equal to {}", i));

                    let vtl = ctx.get_current_vtl();
                    infolog!("vtl: {:?}", vtl);
                    tmk_assert!(vtl == Vtl::Vtl1, format!("vtl should be Vtl0 for VP {}", i));
                    tx.send(());
                },
            ));
            rx.clone().recv();
        }

        // Testing VTL0
        {
            let (mut tx, mut rx) = crate::sync::Channel::new().split();
            ctx.start_on_vp(VpExecutor::new(i, Vtl::Vtl0).command(
                move |ctx: &mut dyn TestCtxTrait| {
                    let vp = ctx.get_current_vp();
                    infolog!("vp: {}", vp);
                    tmk_assert!(vp == i, format!("vp should be equal to {}", i));

                    let vtl = ctx.get_current_vtl();
                    infolog!("vtl: {:?}", vtl);
                    tmk_assert!(vtl == Vtl::Vtl0, format!("vtl should be Vtl0 for VP {}", i));
                    tx.send(());
                },
            ));
            rx.clone().recv();
        }
    }

    criticallog!("All VPs have been tested");
}
