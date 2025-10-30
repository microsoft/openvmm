// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86_64-specific interrupt handling implementation.
//!

use alloc::boxed::Box;

use lazy_static::lazy_static;
use spin::Mutex;
use x86_64::structures::idt::InterruptDescriptorTable;
use x86_64::structures::idt::InterruptStackFrame;

use super::interrupt_handler_register::register_interrupt_handler;
use super::interrupt_handler_register::set_common_handler;

lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        register_interrupt_handler(&mut idt);
        idt.double_fault.set_handler_fn(handler_double_fault);
        idt
    };
}

static mut HANDLERS: [Option<Box<dyn Fn() + 'static>>; 256] = [const { None }; 256];
static MUTEX: Mutex<()> = Mutex::new(());

fn common_handler(_stack_frame: InterruptStackFrame, interrupt: u8) {
    // SAFETY: Handlers are initialized to None and only set via set_handler which is
    // protected by a mutex.
    unsafe {
        if let Some(handler) = &HANDLERS[interrupt as usize] {
            handler()
        }
    }
}

/// Sets the handler for a specific interrupt number.
pub fn set_handler(interrupt: u8, handler: Box<dyn Fn() + 'static>) {
    let _lock = MUTEX.lock();
    // SAFETY: handlers is protected by a mutex.
    unsafe {
        HANDLERS[interrupt as usize] = Some(handler);
    }
}

extern "x86-interrupt" fn handler_double_fault(
    stack_frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    log::error!(
        "EXCEPTION:\n\tERROR_CODE: {}\n\tDOUBLE FAULT\n{:#?}",
        _error_code,
        stack_frame
    );
    loop {
        core::hint::spin_loop();
    }
}

/// Initialize the IDT
pub fn init() {
    IDT.load();
    set_common_handler(common_handler);
    x86_64::instructions::interrupts::enable();
}
