// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86_64-specific interrupt handling.

use alloc::boxed::Box;

use spin::Lazy;
use spin::Mutex;
use x86_64::structures::idt::InterruptDescriptorTable;

use super::interrupt_handler_register::register_interrupt_handler;

static IDT: Lazy<InterruptDescriptorTable> = Lazy::new(|| {
    let mut idt = InterruptDescriptorTable::new();
    register_interrupt_handler(&mut idt);
    idt
});

static mut HANDLERS: [Option<Box<dyn Fn() + 'static>>; 256] = [const { None }; 256];
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Dispatches to the registered handler for `vector`, if any.
///
/// Called from the assembly ISR trampoline with interrupts disabled.
pub(super) fn dispatch(vector: u8) {
    // SAFETY: entries are only published by `set_handler` (serialized by
    // `WRITE_LOCK`, before the corresponding interrupt is armed), and the IDT
    // gates disable interrupts on entry so this read cannot race a writer.
    let handlers = unsafe { &*core::ptr::addr_of!(HANDLERS) };
    if let Some(handler) = handlers[vector as usize].as_ref() {
        handler();
    }
}

/// Sets the handler for a specific interrupt vector.
pub fn set_handler(interrupt: u8, handler: Box<dyn Fn() + 'static>) {
    let _lock = WRITE_LOCK.lock();
    // SAFETY: writers are serialized by `WRITE_LOCK`, and `set_handler` runs
    // during test setup before the corresponding interrupt is armed, so no ISR
    // reads a partially written entry.
    let handlers = unsafe { &mut *core::ptr::addr_of_mut!(HANDLERS) };
    handlers[interrupt as usize] = Some(handler);
}

/// Initializes and loads the IDT and enables interrupts.
pub fn init() {
    IDT.load();
    x86_64::instructions::interrupts::enable();
}
