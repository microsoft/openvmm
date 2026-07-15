// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86_64-specific interrupt handling.

use alloc::boxed::Box;

use spin::Lazy;
use spin::RwLock;
use x86_64::structures::idt::InterruptDescriptorTable;

use super::interrupt_handler_register::register_interrupt_handler;

static IDT: Lazy<InterruptDescriptorTable> = Lazy::new(|| {
    let mut idt = InterruptDescriptorTable::new();
    register_interrupt_handler(&mut idt);
    idt
});

static HANDLERS: RwLock<[Option<Box<dyn Fn() + Send + Sync + 'static>>; 256]> =
    RwLock::new([const { None }; 256]);

/// Dispatches to the registered handler for `vector`, if any.
///
/// Called by ISR handler
pub(super) fn dispatch(vector: u8) {
    let handlers = HANDLERS.read();
    if let Some(handler) = handlers[vector as usize].as_ref() {
        handler();
        return;
    }

    log::error!("unhandled interrupt/exception vector {vector}");

    // Vectors 8 (#DF) and 18 (#MC) cannot be resumed; returning here would
    // `iretq` and almost certainly re-fault into a triple fault (silent reset).
    // Halt instead so the message above survives on the serial log.
    if matches!(vector, 8 | 18) {
        loop {
            x86_64::instructions::hlt();
        }
    }
}

/// Sets the handler for a specific interrupt vector.
pub fn set_handler(interrupt: u8, handler: Box<dyn Fn() + Send + Sync + 'static>) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut handlers = HANDLERS.write();
        handlers[interrupt as usize] = Some(handler);
    });
}

/// Initializes and loads the IDT and enables interrupts.
pub fn init() {
    IDT.load();
    x86_64::instructions::interrupts::enable();
}
