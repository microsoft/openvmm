// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Rust inline asm implementation of the try_* functions for Windows x86_64.
//!
//! This uses manually implemented SEH handlers, avoiding the need to use a C
//! compiler.

// xtask-fmt allow-target-arch sys-crate
#![cfg(all(windows, target_arch = "x86_64"))]

use crate::sys::EXCEPTION_CONTINUE_SEARCH;
use crate::sys::EXCEPTION_EXECUTE_HANDLER;
use crate::AccessFailure;
use windows_sys::Win32::Foundation::EXCEPTION_ACCESS_VIOLATION;

/// The exception filter that runs when there is an access violation in one of
/// the functions defined below.
unsafe extern "C" fn exception_filter(
    pointers: &mut windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
    _frame: *mut (),
) -> i32 {
    // SAFETY: the caller provides a valid pointer to an exception record.
    unsafe {
        if (*pointers.ExceptionRecord).ExceptionCode != EXCEPTION_ACCESS_VIOLATION {
            return EXCEPTION_CONTINUE_SEARCH;
        }
    }

    let rdi;
    let address;
    // SAFETY: the caller provides a valid pointer to an exception record.
    unsafe {
        rdi = (*pointers.ContextRecord).Rdi;
        address = (*pointers.ExceptionRecord).ExceptionInformation[1] as *mut u8;
    }

    // SAFETY: the address of the access failure structure is put in the rdi
    // register before executing the code that may fault.
    unsafe { (rdi as *mut AccessFailure).write(AccessFailure { address }) }
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
            ".p2align 4",
            ".def {func}; .scl 2; .type 32; .endef",
            ".seh_proc {func}",
            "{func}:",
            "push %rdi",
            ".seh_pushreg rdi",
            "sub $32, %rsp",                // space for home params
            ".seh_stackalloc 32",
            ".seh_endprologue",
            // save the failure register to callee-save rdi so that it's available in exception_filter
            concat!("mov ", $failure_reg, ", %rdi"),
            $($head,)*
            "1:",
            $($body,)*
            "2:",
            $($tail,)*
            "3:",
            "add $32, %rsp",
            "pop %rdi",
            "ret",
            "4:",
            "mov $-1, %eax",                // return -1 on failure
            "jmp 3b",
            ".seh_handler __C_specific_handler, @except",
            ".seh_handlerdata",
            ".long 1",                      // one handler entry
            ".long (1b)@IMGREL",            // start address of __try block
            ".long (2b)@IMGREL",            // end address of __try block
            ".long ({filter})@IMGREL",      // exception filter
            ".long (4b)@IMGREL",            // exception handler
            ".text",
            ".seh_endproc",
            ".popsection",
            func = sym $func,
            filter = sym exception_filter,
            options(att_syntax),            // required for IMGREL
        }
    };
}

seh_proc!(
    super::try_memmove,
    "%r9",
    [],
    ["call memcpy", "xorl %eax, %eax"],
    [] // xor is in body since there must be at least one instruction after a call
);
seh_proc!(
    super::try_memset,
    "%r9",
    [],
    ["call memset", "xorl %eax, %eax"],
    [] // xor is in body since there must be at least one instruction after a call
);
seh_proc!(
    super::try_cmpxchg8,
    "%r9",
    ["movb (%rdx), %al"],
    ["cmpxchg %r8b, (%rcx)"],
    ["movb %al, (%rdx)", "setz %al", "movzx %al, %eax"]
);
seh_proc!(
    super::try_cmpxchg16,
    "%r9",
    ["movw (%rdx), %ax",],
    ["cmpxchg %r8w, (%rcx)"],
    ["movw %ax, (%rdx)", "setz %al", "movzx %al, %eax"]
);
seh_proc!(
    super::try_cmpxchg32,
    "%r9",
    ["movl (%rdx), %eax",],
    ["cmpxchg %r8d, (%rcx)"],
    ["movl %eax, (%rdx)", "setz %al", "movzx %al, %eax"]
);
seh_proc!(
    super::try_cmpxchg64,
    "%r9",
    ["movq (%rdx), %rax",],
    ["cmpxchg %r8, (%rcx)"],
    ["movq %rax, (%rdx)", "setz %al", "movzx %al, %eax"]
);
seh_proc!(
    super::try_read8,
    "%r8",
    [],
    ["movb (%rdx), %al"],
    ["movb %al, (%rcx)", "xorl %eax, %eax"]
);
seh_proc!(
    super::try_read16,
    "%r8",
    [],
    ["movw (%rdx), %ax"],
    ["movw %ax, (%rcx)", "xorl %eax, %eax"]
);
seh_proc!(
    super::try_read32,
    "%r8",
    [],
    ["movl (%rdx), %eax"],
    ["movl %eax, (%rcx)", "xorl %eax, %eax"]
);
seh_proc!(
    super::try_read64,
    "%r8",
    [],
    ["movq (%rdx), %rax"],
    ["movq %rax, (%rcx)", "xorl %eax, %eax"]
);
seh_proc!(
    super::try_write8,
    "%r8",
    [],
    ["movb %dl, (%rcx)"],
    ["xorl %eax, %eax"]
);
seh_proc!(
    super::try_write16,
    "%r8",
    [],
    ["movw %dx, (%rcx)"],
    ["xorl %eax, %eax"]
);
seh_proc!(
    super::try_write32,
    "%r8",
    [],
    ["movl %edx, (%rcx)"],
    ["xorl %eax, %eax"]
);
seh_proc!(
    super::try_write64,
    "%r8",
    [],
    ["movq %rdx, (%rcx)"],
    ["xorl %eax, %eax"]
);
