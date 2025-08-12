#![allow(warnings)]
use alloc::alloc::alloc;
use alloc::string::String;
use alloc::sync::Arc;
use core::alloc::GlobalAlloc;
use core::alloc::Layout;
use core::arch::asm;
use core::cell::RefCell;
use core::cell::UnsafeCell;
use core::fmt::Write;
use core::ops::Range;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicI32;
use core::sync::atomic::Ordering;

use ::alloc::boxed::Box;
use ::alloc::vec::Vec;
use context::VpExecutor;
use hvdef::hypercall::HvInputVtl;
use hvdef::HvAllArchRegisterName;
use hvdef::HvRegisterVsmVpStatus;
use hvdef::HvX64RegisterName;
use hvdef::Vtl;
use hypvctx::HvTestCtx;
use iced_x86::DecoderOptions;
use iced_x86::Formatter;
use iced_x86::NasmFormatter;
use sync_nostd::Channel;
use sync_nostd::Receiver;
use sync_nostd::Sender;
use uefi::entry;
use uefi::Status;

use crate::context;
use crate::context::InterruptPlatformTrait;
use crate::context::SecureInterceptPlatformTrait;
use crate::context::VirtualProcessorPlatformTrait;
use crate::context::VtlPlatformTrait;
use crate::platform::hypvctx;
use crate::tmk_assert;
use crate::tmk_logger;
use crate::tmkdefs::TmkResult;

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

pub fn read_assembly_output(target: u64) -> usize {
    unsafe {
        let target_ptr = target as *const u8;
        let code_bytes = core::slice::from_raw_parts(target_ptr, 0x100);
        let mut decoder = iced_x86::Decoder::with_ip(64, code_bytes, target, DecoderOptions::NONE);

        let mut formatter = NasmFormatter::new();
        let mut output = String::new();
        let mut first_ip_len = 0;
        let mut set = false;
        while decoder.can_decode() {
            let instr = decoder.decode();
            if !set {
                first_ip_len = instr.len();
                set = true;
            }
            formatter.format(&instr, &mut output);
            log::info!("{}:{}", instr.ip(), output);
            output.clear();
        }

        first_ip_len
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
            let mut hv = HvTestCtx::new();
            // expected to get interrupt in VTL1.
            // CVMs dont support hypercalls to get the current VTL from VTL1/0.
            hv.init(Vtl::Vtl1);
            log::info!(
                "current vp from interrupt: {}",
                hv.get_current_vp().unwrap()
            );

            let rip = hvdef::HvX64RegisterName::Rip.0;

            let reg = hv.get_vp_state_with_vtl(rip, Vtl::Vtl0);
            tmk_assert!(reg.is_ok(), "get_vp_state_with_vtl should succeed");

            let reg = reg.unwrap();
            log::info!("rip from vtl0: 0x{:x}", reg);

            log::info!("pring assembly for the current RIP:");
            let size = read_assembly_output(reg);

            let new_rip_value = reg + size as u64;

            log::info!("pring assembly for the updated RIP:");
            read_assembly_output(new_rip_value);

            let r = hv.set_vp_state_with_vtl(HvX64RegisterName::Rip.0, new_rip_value, Vtl::Vtl0);
            tmk_assert!(r.is_ok(), "set_vp_state_with_vtl should succeed");

            let reg = hv.get_vp_state_with_vtl(rip, Vtl::Vtl0);
            tmk_assert!(reg.is_ok(), "get_vp_state_with_vtl should succeed");

            let reg = reg.unwrap();
            log::info!("rip from vtl0 after modification: 0x{:x}", reg);
            tmk_assert!(reg == new_rip_value, "rip should be modified");

            log::info!("pring assembly for the updated RIP after fetch:");
            read_assembly_output(reg);

            log::info!("interrupt handled!");
            hv.print_rbp();
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

    ctx.start_on_vp(VpExecutor::new(0x2, Vtl::Vtl1).command(move |ctx: &mut T| {
        ctx.setup_interrupt_handler();
        ctx.setup_secure_intercept(0x30);

        log::info!("successfully started running VTL1 on vp2.");
    }));

    ctx.start_on_vp(
        VpExecutor::new(0x2, Vtl::Vtl0).command(move |ctx: &mut T| unsafe {
            log::info!("successfully started running VTL0 on vp2.");

            ctx.queue_command_vp(VpExecutor::new(2, Vtl::Vtl1).command(move |ctx: &mut T| {
                log::info!("after intercept successfully started running VTL1 on vp2.");
                ctx.switch_to_low_vtl();
            }));

            backup_and_restore();
            log::info!(
                "reading mutated heap memory from vtl0(it should not be 0xA2): 0x{:x}",
                RETURN_VALUE
            );
            tmk_assert!(
                RETURN_VALUE != 0xA2,
                "heap memory should not be accessible from vtl0"
            );
            tx.send(());
        }),
    );

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
