#![allow(warnings)]
use alloc::{alloc::alloc, sync::Arc};
use core::{
    alloc::{GlobalAlloc, Layout}, arch::asm, cell::{RefCell, UnsafeCell}, fmt::Write, ops::Range, sync::atomic::{AtomicBool, AtomicI32, Ordering}
};

use ::alloc::{boxed::Box, vec::Vec};
use context::VpExecutor;
use hvdef::{
    hypercall::HvInputVtl, HvAllArchRegisterName, HvRegisterVsmVpStatus, HvX64RegisterName, Vtl,
};
use hypvctx::HvTestCtx;
use sync_nostd::{Channel, Receiver, Sender};
use uefi::{entry, Status};

use crate::{tmk_assert, tmk_logger};
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
static mut RETURN_VALUE: u8 = 0;
#[inline(never)]
fn violate_heap() {
    unsafe {
        let heapx = *HEAPX.borrow();
        RETURN_VALUE = *(heapx.add(10));
    }
}

#[inline(never)]
fn backup_and_restore() {
    use core::arch::asm;
    unsafe {
        asm!("
            push rax
            push rbx
            push rcx
            push rdx
            push rsi
            push rdi
            push rbp
            push r8
            push r9
            push r10
            push r11
            push r12
            push r13
            push r14
            push r15
            call {}
            pop r15
            pop r14
            pop r13
            pop r12
            pop r11
            pop r10
            pop r9
            pop r8
            pop rbp
            pop rdi
            pop rsi
            pop rdx
            pop rcx
            pop rbx
            pop rax
        ", sym violate_heap);
    }
}


pub fn exec<T>(ctx: &mut T)
where
    T: InterruptPlatformTrait
        + VtlPlatformTrait
        + VirtualProcessorPlatformTrait<T>,
{
    log::info!("ctx ptr: {:p}", &ctx as *const _);

    let vp_count = ctx.get_vp_count();
    tmk_assert!(vp_count.is_ok(), "get_vp_count should succeed");
    let vp_count = vp_count.unwrap();
    tmk_assert!(vp_count == 4, "vp count should be 8");

    ctx.setup_interrupt_handler();
    log::info!("successfully setup interrupt handler");

    ctx.setup_partition_vtl(Vtl::Vtl1);
    log::info!("successfully setup partition vtl1");
    
    ctx.start_on_vp(VpExecutor::new(0, Vtl::Vtl1).command(move |ctx: &mut T| {
        log::info!("successfully started running VTL1 on vp0.");

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

        let result = ctx.apply_vtl_protection_for_memory(range, Vtl::Vtl1);
        tmk_assert!(result.is_ok(), "apply_vtl_protection_for_memory should succeed");

        log::info!("moving to vtl0 to attempt to read the heap memory");

        ctx.switch_to_low_vtl();
    }));

    log::info!("BACK to vtl0");
    ctx.set_interrupt_idx(18, || {
        tmk_assert!(true, "we reached to MC handler");
        panic!("MC causes the test to end");
    });

    let (tx, rx) = Channel::new().split();
    
    ctx.start_on_vp(VpExecutor::new(0x2, Vtl::Vtl1).command(move|ctx: &mut T| {
        ctx.setup_interrupt_handler();
        log::info!("successfully started running VTL1 on vp2.");
    }));

    ctx.start_on_vp(VpExecutor::new(0x2, Vtl::Vtl0).command( move |ctx: &mut T| unsafe {        
        log::info!("successfully started running VTL0 on vp2.");
        unsafe {
            let heapx = *HEAPX.borrow();

            let read_protected_memory = || { *(heapx.add(10)) };

            let read_result = read_protected_memory();
            log::info!(
                "reading mutated heap memory from vtl0(it should not be 0xA2): 0x{:x}",
                read_result
            );
            tmk_assert!(   

                 
                read_result != 0xA2,
                "heap memory should not be accessible from vtl0"
            );
        }

        tx.send(());
    }));

    rx.recv();

    tmk_assert!(false, "we should not reach here injecting MC should terminate the test");
}
