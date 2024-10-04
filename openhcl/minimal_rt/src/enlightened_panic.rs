// Copyright (C) Microsoft Corporation. All rights reserved.

//! Enlightened panic for a Hyper-V guest.

//! Hyper-V guests may choose to report a crash to the Hyper-V host
//! via a set of MSRs (x64) or synthetic crash registers on ARM.
//! The registers carry crash-specific information and may optionally
//! include a message buffer.

use crate::arch::write_crash_reg;
use arrayvec::ArrayString;
use core::fmt::Write;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::Ordering::Relaxed;
use hvdef::GuestCrashCtl;

// This is chosen to identify the boot shim as a pre-OS environment
const PRE_OS_ID: u8 = 5;

static CAN_REPORT_MSG: AtomicBool = AtomicBool::new(false);

fn report_raw(msg: &[u8], msg_pa: Option<usize>) {
    // Before using the guest crash MSRs, could check
    // if these are supported. Here, we don't do that
    // as the intention is to fault anyways.

    let crash_ctl = GuestCrashCtl::new()
        .with_pre_os_id(PRE_OS_ID)
        .with_no_crash_dump(true)
        .with_crash_message(!msg.is_empty())
        .with_crash_notify(true);

    // SAFETY: Using the contract established in the Hyper-V TLFS.
    unsafe {
        write_crash_reg(0, u64::from_le_bytes(*b"BOOTSHIM"));
        write_crash_reg(1, u64::from_be_bytes(*b"IGVMBOOT"));
        write_crash_reg(2, u64::MAX);

        match (msg, msg_pa) {
            (msg @ [_, ..], Some(msg_pa)) => {
                // There is a non-empty message and a valid physical address.
                write_crash_reg(3, msg_pa as u64);
                write_crash_reg(4, msg.len() as u64);
            }
            _ => {
                write_crash_reg(3, 0);
                write_crash_reg(4, 0);
            }
        }

        // Report crash to Hyper-V
        write_crash_reg(5, crash_ctl.into());
    }
}

/// Reports the panic.
///
/// `stack_va_to_pa` takes an object on the stack and returns its physical address.
pub fn report(
    panic: &core::panic::PanicInfo<'_>,
    mut stack_va_to_pa: impl FnMut(*const ()) -> Option<usize>,
) {
    let mut panic_buffer = ArrayString::<512>::new();
    if CAN_REPORT_MSG.load(Relaxed) {
        let _ = write!(panic_buffer, "{}", panic);
    }
    report_raw(
        panic_buffer.as_ref().as_bytes(),
        stack_va_to_pa(panic_buffer.as_bytes().as_ptr().cast()),
    );
}

/// Enables writing the enlightened panic message.
///
/// If this is not called, then [`report`] will just report that a panic occurred
/// and not include the panic message.
pub fn enable_enlightened_panic() {
    CAN_REPORT_MSG.store(true, Relaxed);
}
