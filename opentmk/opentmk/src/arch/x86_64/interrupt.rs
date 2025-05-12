use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};
use lazy_static::lazy_static;
use sync_nostd::Mutex;

use super::interrupt_handler_register::{register_interrupt_handler, set_common_handler};

lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        register_interrupt_handler(&mut idt);
        idt.double_fault.set_handler_fn(handler_double_fault);
        idt
    };
}

static mut HANDLERS : [fn(); 256] = [no_op; 256];
static MUTEX: Mutex<()> = Mutex::new(());
fn no_op() {}

fn common_handler(_stack_frame: InterruptStackFrame, interrupt: u8) {
    unsafe { HANDLERS[interrupt as usize](); }
}

pub fn set_handler(interrupt: u8, handler: fn()) {
    let _lock = MUTEX.lock();
    unsafe { HANDLERS[interrupt as usize] = handler; }
}


extern "x86-interrupt" fn handler_double_fault(
    stack_frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    log::error!("EXCEPTION:\n\tERROR_CODE: {}\n\tDOUBLE FAULT\n{:#?}", _error_code, stack_frame);
    loop {}
}

// Initialize the IDT
pub fn init() {
    IDT.load();
    set_common_handler(common_handler);
    x86_64::instructions::interrupts::enable();
}