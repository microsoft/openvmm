// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use hvdef::Vtl;
use nostd_spin_channel::Channel;

use crate::context::VirtualProcessorPlatformTrait;
use crate::context::VpExecToken;
use crate::context::VtlPlatformTrait;
use crate::tmk_assert;

/// Executes a series of negitive tests to validate VTL and VP functionalities.
pub fn exec<T>(ctx: &mut T)
where
    T: VtlPlatformTrait + VirtualProcessorPlatformTrait<T>,
{
    // Skipping VTL setup for now to test the negative case

    let vp_count = ctx.get_vp_count();
    tmk_assert!(vp_count.is_ok(), "get_vp_count should succeed");

    let vp_count = vp_count.unwrap();
    tmk_assert!(vp_count == 4, "vp count should be 4");

    // Testing BSP VTL1 Bringup
    {
        let (tx, _rx) = Channel::new().split();

        let result = ctx.start_on_vp(VpExecToken::new(0, Vtl::Vtl1).command(move |ctx: &mut T| {
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
            tx.send(())
                .expect("Failed to send message through the channel");
            ctx.switch_to_low_vtl();
        }));

        tmk_assert!(result.is_err(), "start_on_vp should fail");
        tmk_assert!(
            result.unwrap_err() == crate::tmkdefs::TmkError::InvalidVtlState,
            "start_on_vp should fail with InvalidVtlState"
        );
        log::info!("result on start_on_vp: {:?}", result);
    }
}
