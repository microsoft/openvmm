#![allow(warnings)]
use alloc::{alloc::alloc, sync::Arc};
use core::{
    alloc::{GlobalAlloc, Layout},
    arch::asm,
    cell::{RefCell, UnsafeCell},
    ops::Range,
    sync::atomic::{AtomicBool, AtomicI32, Ordering},
};

use ::alloc::{boxed::Box, vec::Vec};
use context::VpExecutor;
use hvdef::{
    hypercall::HvInputVtl, HvAllArchRegisterName, HvRegisterVsmVpStatus, HvX64RegisterName, Vtl,
};
use hypvctx::HvTestCtx;
use sync_nostd::{Channel, Receiver, Sender};
use uefi::{entry, Status};

// WIP : This test is not yet complete and is not expected to pass.
//
// This test is to verify that the VTL protections are working as expected.
// The stack values in VTL0 are changing after interrupt handling in VTL1.
use crate::tmk_assert;
use crate::{
    context,
    context::{
        InterruptPlatformTrait, SecureInterceptPlatformTrait, VirtualProcessorPlatformTrait,
        VtlPlatformTrait,
    },
    platform::hypvctx,
    tmkdefs::TmkResult,
};

static mut HEAPX: RefCell<*mut u8> = RefCell::new(0 as *mut u8);
static mut CON: AtomicI32 = AtomicI32::new(0);

fn call_act() {
    unsafe {
        let heapx = *HEAPX.borrow();
        let val = *(heapx.add(10));
        log::info!(
            "reading mutated heap memory from vtl0(it should not be 0xAA): 0x{:x}",
            val
        );
        tmk_assert!(
            val != 0xAA,
            "heap memory should not be accessible from vtl0"
        );
    }
}

pub fn exec<T>(ctx: &mut T)
where
    T: InterruptPlatformTrait
        + SecureInterceptPlatformTrait
        + VtlPlatformTrait
        + VirtualProcessorPlatformTrait<T>,
{
    log::info!("ctx ptr: {:p}", &ctx as *const _);

    let vp_count = ctx.get_vp_count();
    tmk_assert!(vp_count.is_ok(), "get_vp_count should succeed");
    let vp_count = vp_count.unwrap();
    tmk_assert!(vp_count == 4, "vp count should be 8");

    ctx.setup_interrupt_handler();

    log::info!("set intercept handler successfully!");

    ctx.setup_partition_vtl(Vtl::Vtl1);

    ctx.start_on_vp(VpExecutor::new(0, Vtl::Vtl1).command(move |ctx: &mut T| {
        log::info!("successfully started running VTL1 on vp0.");
        ctx.setup_secure_intercept(0x30);
        ctx.set_interrupt_idx(0x30, move || {
            log::info!("interrupt fired!");

            let hv = HvTestCtx::new();
            log::info!(
                "current vp from interrupt: {}",
                hv.get_current_vp().unwrap()
            );

            log::info!("interrupt handled!");
        });

        let layout =
            Layout::from_size_align(1024 * 1024, 4096).expect("msg: failed to create layout");
        let ptr = unsafe { alloc(layout) };
        log::info!("allocated some memory in the heap from vtl1");
        unsafe {
            let mut z = HEAPX.borrow_mut();
            *z = ptr;
            *ptr.add(10) = 0xA2;
        }

        let size = layout.size();
        ctx.setup_vtl_protection();

        log::info!("enabled vtl protections for the partition.");

        let range = Range {
            start: ptr as u64,
            end: ptr as u64 + size as u64,
        };

        ctx.apply_vtl_protection_for_memory(range, Vtl::Vtl1);

        log::info!("moving to vtl0 to attempt to read the heap memory");

        ctx.switch_to_low_vtl();
    }));

    log::info!("ctx ptr: {:p}", &ctx as *const _);

    let mut l = 0u64;
    unsafe { asm!("mov {}, rsp", out(reg) l) };
    log::info!("rsp: 0x{:x}", l);

    let (tx, rx) = Channel::new().split();
    
    ctx.start_on_vp(VpExecutor::new(0x2, Vtl::Vtl1).command(move|ctx: &mut T| {
        ctx.setup_interrupt_handler();
        ctx.setup_secure_intercept(0x30);

        log::info!("successfully started running VTL1 on vp2.");
    }));

    ctx.start_on_vp(VpExecutor::new(0x2, Vtl::Vtl0).command( move |ctx: &mut T| unsafe {        
        
        log::info!("successfully started running VTL0 on vp2.");

        ctx.queue_command_vp(VpExecutor::new(2, Vtl::Vtl1).command(move |ctx: &mut T| {
            log::info!("after intercept successfully started running VTL1 on vp2.");
            ctx.switch_to_low_vtl();
        }));

        unsafe {
            let heapx = *HEAPX.borrow();
            let val = *(heapx.add(10));
            log::info!(
                "reading mutated heap memory from vtl0(it should not be 0xAA): 0x{:x}",
                val
            );
            tmk_assert!(
                val != 0xAA,
                "heap memory should not be accessible from vtl0"
            );
        }

        tx.send(());
    }));

    rx.recv();
    // let (mut tx, mut rx) = Channel::new(1);
    // {
    //     let mut tx = tx.clone();
    //     ctx.start_on_vp(VpExecutor::new(2, Vtl::Vtl0).command(
    //         move |ctx: &mut dyn TestCtxTrait| {
    //             log::info!("Hello form vtl0 on vp2!");
    //             tx.send(());
    //         },
    //     ));
    // }

    // rx.recv();

    log::info!("we are in vtl0 now!");
    log::info!("we reached the end of the test");
}
