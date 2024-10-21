// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Rust inline asm implementation of the try_* functions for Windows ARM64.
//!
//! This uses manually implemented SEH handlers, avoiding the need to use a C
//! compiler.

// xtask-fmt allow-target-arch sys-crate
#![cfg(all(windows, target_arch = "aarch64"))]

use crate::sys::EXCEPTION_CONTINUE_SEARCH;
use crate::sys::EXCEPTION_EXECUTE_HANDLER;
use crate::AccessFailure;
use windows_sys::Win32::Foundation::EXCEPTION_ACCESS_VIOLATION;

/// The exception filter that runs when there is an access violation in one of
/// the functions defined below.
unsafe extern "C" fn exception_filter(
    pointers: &mut windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
    frame: *mut (),
) -> i32 {
    // SAFETY: the caller provides a valid pointer to an exception record.
    unsafe {
        if (*pointers.ExceptionRecord).ExceptionCode != EXCEPTION_ACCESS_VIOLATION {
            return EXCEPTION_CONTINUE_SEARCH;
        }
    }

    let af_addr;
    let address;
    // SAFETY: the caller provides a valid pointer to an exception record.
    unsafe {
        // Number `8` comes from the stack frame allocation code in `seh_proc!` below.
        af_addr = frame.cast_const().cast::<u8>().sub(8).cast::<u64>().read();
        address = (*pointers.ExceptionRecord).ExceptionInformation[1] as *mut u8;
    }

    // SAFETY: the address of the access failure structure is put onto the stack
    // before executing the code that may fault.
    unsafe { (af_addr as *mut AccessFailure).write(AccessFailure { address }) }
    EXCEPTION_EXECUTE_HANDLER
}

/// Defines a function with a __try/__except block. `$head` runs before the try,
/// `$body` runs inside the try, and `$tail` runs after the try.
///
/// If code faults while running the instructions in `$body`, then the exception
/// filter will fill out the [`AccessFailure`] pointed to by `$failure_reg`, and
/// the function will return -1.
macro_rules! seh_proc {
    ($func:path, $failure_reg:expr, [$($head:expr),* $(,)?], [$($body:expr),* $(,)?], [$($tail:expr),* $(,)?]) => {
        std::arch::global_asm! {
            ".pushsection .text",
            ".globl {func}",
            ".arch armv8.1-a",
            ".p2align 2",
            ".def {func}; .scl 2; .type 32; .endef",
            ".seh_proc {func}",
            "{func}:",
            "sub sp, sp, #48",
            ".seh_stackalloc 48",
            "stp fp, lr, [sp, #16]",
            ".seh_save_fplr	16",
            "add fp, sp, #16",
            ".seh_add_fp 16",
            ".seh_endprologue",
            // Save the pointer to the access failure data on the stack
            // for the exception filter.
            concat!("str ", $failure_reg, ", [fp, #-8]"),
            $($head,)*
            "1:",
            $($body,)*
            "2:",
            $($tail,)*
            "3:",
            ".seh_startepilogue",
            "ldp fp, lr, [sp, #16]",
            ".seh_save_fplr 16",
            "add sp, sp, #48",
            ".seh_stackalloc 48",
            ".seh_endepilogue",
            "ret",
            "4:",
            "mov w0, #-1",                // return -1 on failure
            "b 3b",
            ".seh_handler __C_specific_handler, @except",
            ".seh_handlerdata",
            ".long 1",                    // one handler entry
            ".long (1b)@IMGREL",          // start address of __try block
            ".long (2b)@IMGREL",          // end address of __try block
            ".long ({filter})@IMGREL",    // exception filter
            ".long (4b)@IMGREL",          // exception handler
            ".text",
            ".seh_endproc",
            ".popsection",
            func = sym $func,
            filter = sym exception_filter,
        }
    };
}

seh_proc!(
    super::try_memmove,
    "x3",
    [],
    ["bl memcpy", "mov w0, wzr"],
    [] // mov is in body since there must be at least one instruction after a call
);
seh_proc!(
    super::try_memset,
    "x3",
    [],
    ["bl memset", "mov w0, wzr"],
    [] // mov is in body since there must be at least one instruction after a call
);
seh_proc!(
    super::try_cmpxchg8,
    "x3",
    ["ldrb w8, [x1]", "mov w9, w8"],
    ["casalb w8, w2, [x0]"],
    ["strb w8, [x1]", "cmp w8, w9", "cset w0, eq"]
);
seh_proc!(
    super::try_cmpxchg16,
    "x3",
    ["ldrh w8, [x1]", "mov w9, w8"],
    ["casalh w8, w2, [x0]"],
    ["strh w8, [x1]", "cmp w8, w9", "cset w0, eq"]
);
seh_proc!(
    super::try_cmpxchg32,
    "x3",
    ["ldr w8, [x1]", "mov w9, w8"],
    ["casal w8, w2, [x0]"],
    ["str w8, [x1]", "cmp w8, w9", "cset w0, eq"]
);
seh_proc!(
    super::try_cmpxchg64,
    "x3",
    ["ldr x8, [x1]", "mov x9, x8"],
    ["casal x8, x2, [x0]"],
    ["str x8, [x1]", "cmp x8, x9", "cset w0, eq"]
);
seh_proc!(
    super::try_read8,
    "x2",
    [],
    ["ldrb w8, [x1]"],
    ["strb w8, [x0]", "mov w0, wzr"]
);
seh_proc!(
    super::try_read16,
    "x2",
    [],
    ["ldrh w8, [x1]"],
    ["strh w8, [x0]", "mov w0, wzr"]
);
seh_proc!(
    super::try_read32,
    "x2",
    [],
    ["ldr w8, [x1]"],
    ["str w8, [x0]", "mov w0, wzr"]
);
seh_proc!(
    super::try_read64,
    "x2",
    [],
    ["ldr x8, [x1]"],
    ["str x8, [x0]", "mov w0, wzr"]
);
seh_proc!(
    super::try_write8,
    "x2",
    [],
    ["strb w1, [x0]"],
    ["mov w0, wzr"]
);
seh_proc!(
    super::try_write16,
    "x2",
    [],
    ["strh w1, [x0]"],
    ["mov w0, wzr"]
);
seh_proc!(
    super::try_write32,
    "x2",
    [],
    ["str w1, [x0]"],
    ["mov w0, wzr"]
);
seh_proc!(
    super::try_write64,
    "x2",
    [],
    ["str x1, [x0]"],
    ["mov w0, wzr"]
);
