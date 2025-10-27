// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use alloc::alloc::alloc;
use core::alloc::Layout;
use core::arch::asm;
use core::cell::RefCell;
use core::ops::Range;

use hvdef::Vtl;
use nostd_spin_channel::Channel;
use spin::Mutex;

use crate::context::InterruptPlatformTrait;
use crate::context::SecureInterceptPlatformTrait;
use crate::context::VirtualProcessorPlatformTrait;
use crate::context::VpExecToken;
use crate::context::VtlPlatformTrait;
use crate::create_function_with_restore;
use crate::tmk_assert;

static mut HEAP_ALLOC_PTR: RefCell<*mut u8> = RefCell::new(0 as *mut u8);
static FAULT_CALLED: Mutex<bool> = Mutex::new(false);

// Without inline the compiler may optimize away the call and the VTL switch may
// distort the architectural registers
#[inline(never)]
// SAFETY: we safely handle HEAP_ALLOC_PTR so ignoring it here.
#[expect(static_mut_refs)]
fn violate_heap() {
    unsafe {
        let alloc_ptr = *HEAP_ALLOC_PTR.borrow();
        *(alloc_ptr.add(10)) = 0x56;
    }
}
create_function_with_restore!(f_violate_heap, violate_heap);

/// Executes a series of tests to validate memory protection between VTLs.
pub fn exec<T>(ctx: &mut T)
where
    T: InterruptPlatformTrait
        + SecureInterceptPlatformTrait
        + VtlPlatformTrait
        + VirtualProcessorPlatformTrait<T>,
{
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

        let layout =
            Layout::from_size_align(1024 * 1024, 4096).expect("msg: failed to create layout");
        // SAFETY: we are allocating memory to heap, we don't free it in this test.
        let ptr = unsafe { alloc(layout) };
        log::info!("allocated some memory in the heap from vtl1");

        #[expect(static_mut_refs)]
        unsafe {
            let mut z = HEAP_ALLOC_PTR.borrow_mut();
            *z = ptr;
            *ptr.add(10) = 0xA2;
        }

        let size = layout.size();
        let r = ctx.setup_vtl_protection();
        tmk_assert!(r.is_ok(), "setup_vtl_protection should succeed");
        log::info!("enabled vtl protections for the partition.");

        let range = Range {
            start: ptr as u64,
            end: ptr as u64 + size as u64,
        };

        let r = ctx.apply_vtl_protection_for_memory(range, Vtl::Vtl1);
        tmk_assert!(r.is_ok(), "apply_vtl_protection_for_memory should succeed");

        log::info!("moving to vtl0 to attempt to read the heap memory");

        ctx.switch_to_low_vtl();
    }));
    tmk_assert!(r.is_ok(), "start_on_vp should succeed");

    let (tx, rx) = Channel::new().split();

    let r = ctx.start_on_vp(
        VpExecToken::new(0x2, Vtl::Vtl1).command(move |ctx: &mut T| {
            let r = ctx.setup_interrupt_handler();
            tmk_assert!(r.is_ok(), "setup_interrupt_handler should succeed");

            let r = ctx.setup_secure_intercept(0x30);
            tmk_assert!(r.is_ok(), "setup_secure_intercept should succeed");

            log::info!("successfully started running VTL1 on vp2.");
        }),
    );
    tmk_assert!(r.is_ok(), "start_on_vp should succeed");

    let r = ctx.start_on_vp(
        VpExecToken::new(0x2, Vtl::Vtl0).command(move |ctx: &mut T| {
            log::info!("successfully started running VTL0 on vp2.");

            let r =
                ctx.queue_command_vp(VpExecToken::new(2, Vtl::Vtl1).command(move |ctx: &mut T| {
                    log::info!("after intercept successfully started running VTL1 on vp2.");
                    ctx.switch_to_low_vtl();
                }));
            tmk_assert!(r.is_ok(), "queue_command_vp should succeed");

            f_violate_heap();
            _ = tx.send(());
        }),
    );
    tmk_assert!(r.is_ok(), "start_on_vp should succeed");

    _ = rx.recv();

    let fault_called = *FAULT_CALLED.lock();
    tmk_assert!(fault_called, "Secure intercept should be received");

    log::info!("we are in vtl0 now!");
    log::info!("we reached the end of the test");
}
