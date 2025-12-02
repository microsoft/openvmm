// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::arch::asm;

use spin::Mutex;

use crate::context::InterruptPlatformTrait;
use crate::context::SecureInterceptPlatformTrait;
use crate::context::VirtualProcessorPlatformTrait;
use crate::context::VtlPlatformTrait;
use crate::create_function_with_restore;
use crate::tmk_assert;

static FAULT_CALLED: Mutex<bool> = Mutex::new(false);

// Without inline the compiler may optimize away the call and the VTL switch may
// distort the architectural registers
#[inline(never)]
#[cfg(target_arch = "x86_64")] // xtask-fmt allow-target-arch cpu-intrinsic
fn violate_reg_rule() {
    // SAFETY: we are writing to a valid MSR
    unsafe {
        asm!(
            "mov ecx, 0x1B",
            "wrmsr",
            out("eax") _,
            out("edx") _,
            out("ecx") _,
        );
    }
}

// The macro `create_function_with_restore!` generates a wrapper function (`f_violate_reg_rule`)
// that calls `violate_reg_rule` and restores the processor state as needed for virtualization tests.
// Usage: create_function_with_restore!(wrapper_fn_name, target_fn_name);
create_function_with_restore!(f_violate_reg_rule, violate_reg_rule);

/// Executes a series of tests to validate secure register intercept functionality.
pub fn exec<T>(ctx: &mut T)
where
    T: InterruptPlatformTrait
        + SecureInterceptPlatformTrait
        + VtlPlatformTrait
        + VirtualProcessorPlatformTrait<T>,
{
    use hvdef::Vtl;

    use crate::context::VpExecToken;

    let vp_count = ctx.get_vp_count();
    tmk_assert!(vp_count.is_ok(), "get_vp_count should succeed");
    let vp_count = vp_count.unwrap();
    tmk_assert!(vp_count == 4, "vp count should be 4");

    let r = ctx.setup_interrupt_handler();
    tmk_assert!(r.is_ok(), "setup_interrupt_handler should succeed");
    log::info!("set intercept handler successfully!");

    let r = ctx.setup_partition_vtl(Vtl::Vtl1);
    tmk_assert!(r.is_ok(), "setup_partition_vtl should succeed");

    let r = ctx.start_on_vp(VpExecToken::new(0, Vtl::Vtl1).command(move |ctx: &mut T| {
        log::info!("successfully started running VTL1 on vp0.");
        let r = ctx.setup_secure_intercept(0x30);
        tmk_assert!(r.is_ok(), "setup_secure_intercept should succeed");

        let r = ctx.set_interrupt_idx(0x30, move |mut ctx| {
            log::info!("interrupt handled for 0x30!");
            let mut status = FAULT_CALLED.lock();
            *status = true;
            let r = ctx.signal_intercept_handled();
            tmk_assert!(r.is_ok(), "signal_intercept_handled should succeed");
        });
        tmk_assert!(r.is_ok(), "set_interrupt_idx should succeed");

        let r = ctx.set_register(0x000E0000, 0x0000000000001000);
        tmk_assert!(r.is_ok(), "set_register should succeed to write Control register");

        let r = ctx.get_register(0x000E0000);
        tmk_assert!(r.is_ok(), "get_register should succeed to read Control register");

        let reg_values = r.unwrap();
        tmk_assert!(reg_values == 0x0000000000001000, format!("register value should be 0x0000000000001000, got {:x}", reg_values));

        log::info!("Switching to VTL0: attempting to read a protected register to verify security enforcement and intercept handling.");

        ctx.switch_to_low_vtl();
    }));
    tmk_assert!(r.is_ok(), "start_on_vp should succeed");

    _ = ctx.queue_command_vp(VpExecToken::new(0x0, Vtl::Vtl1).command(|ctx: &mut T| {
        log::info!("successfully resumed running VTL1 on vp0 after intercept");
        ctx.switch_to_low_vtl();
    }));

    f_violate_reg_rule();

    let fault_called = *FAULT_CALLED.lock();
    tmk_assert!(fault_called, "Secure intercept should be received");

    log::info!("we are in vtl0 now!");
    log::info!("we reached the end of the test");
}
