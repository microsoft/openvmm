#![allow(warnings)]
// WIP : This test is not yet complete and is not expected to pass.
//
// This test is to verify that the VTL protections are working as expected.
// The stack values in VTL0 are changing after interrupt handling in VTL1.
use crate::tmk_assert::{AssertOption, AssertResult};
use crate::tmkdefs::TmkResult;
use sync_nostd::{Channel, Receiver, Sender};
use crate::uefi::alloc::{ALLOCATOR, SIZE_1MB};
use crate::{context, uefi::hypvctx};
use crate::{tmk_assert};
use ::alloc::boxed::Box;
use alloc::sync::Arc;
use ::alloc::vec::Vec;
use context::{TestCtxTrait, VpExecutor};
use hypvctx::HvTestCtx;
use core::alloc::{GlobalAlloc, Layout};
use core::arch::asm;
use core::cell::RefCell;
use core::ops::Range;
use core::sync::atomic::{AtomicI32, Ordering};
use hvdef::hypercall::HvInputVtl;
use hvdef::{HvAllArchRegisterName, HvRegisterVsmVpStatus, HvX64RegisterName, Vtl};
use uefi::entry;
use uefi::Status;

static mut HEAPX: RefCell<*mut u8> = RefCell::new(0 as *mut u8);
static mut CON: AtomicI32 = AtomicI32::new(0);

pub fn exec<T>(ctx: &mut T) where T: TestCtxTrait<T> {
    log::info!("ctx ptr: {:p}", &ctx as *const _);

    let mut vp_count = ctx.get_vp_count();
    tmk_assert!(vp_count == 8, "vp count should be 8");

    ctx.setup_interrupt_handler();

    log::info!("set intercept handler successfully!");

    ctx.setup_partition_vtl(Vtl::Vtl1);

    ctx.start_on_vp(
        VpExecutor::new(0, Vtl::Vtl1).command(move |ctx: &mut T|{
            log::info!("successfully started running VTL1 on vp0.");
            ctx.setup_secure_intercept(0x30);
            ctx.set_interrupt_idx(0x30, || {
                log::info!("interrupt fired!");

                let mut hv_test_ctx = HvTestCtx::new();
                hv_test_ctx.init();

                let c = hv_test_ctx.get_register(HvAllArchRegisterName::VsmVpStatus.0);
                tmk_assert!(c.is_ok(), "read should succeed");
                
                let c = c.unwrap();
                let cp = HvRegisterVsmVpStatus::from_bits(c as u64);

                log::info!("VSM VP Status: {:?}", cp);

                log::info!("interrupt handled!");
            });

            let layout =
                Layout::from_size_align(SIZE_1MB, 4096).expect("msg: failed to create layout");
            let ptr = unsafe { ALLOCATOR.alloc(layout) };
            log::info!("allocated some memory in the heap from vtl1");
            unsafe {
                let mut z = HEAPX.borrow_mut();
                *z = ptr;
                *ptr.add(10) = 0xAA;
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
        }),
    );

    ctx.queue_command_vp(VpExecutor::new(0, Vtl::Vtl1).command(move |ctx: &mut T| {
        log::info!("successfully started running VTL1 on vp0.");
        ctx.switch_to_low_vtl();
    }));
    log::info!("ctx ptr: {:p}", &ctx as *const _);

    let mut l = 0u64;
    unsafe { asm!("mov {}, rsp", out(reg) l) };
    log::info!("rsp: 0x{:x}", l);
    unsafe {
        log::info!("Attempting to read heap memory from vtl0");
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

    unsafe { asm!("mov {}, rsp", out(reg) l) };
    log::info!("rsp: 0x{:x}", l);

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
    log::info!("ctx ptr: {:p}", &ctx as *const _);
    let c = ctx.get_vp_count();

    tmk_assert!(c == 8, "vp count should be 8");

    // rx.recv();

    log::info!("we are in vtl0 now!");
    log::info!("we reached the end of the test");
    loop {
        
    }
    
}