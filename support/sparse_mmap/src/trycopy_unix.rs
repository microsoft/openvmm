// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Rust inline asm implementation of the try_* functions for Linux-based OS'es
//! and macOS.
//!
//! There is a popular crate, `signal-hook`, but it seems to enforce the model
//! where a thread has to (a)wait and then iterate through the received signals.
//! Here, the code handles the signals in the "runtimeless" async way where
//! the OS runs the handler by interrupting any thread in the process.
//!
//! This uses a special stack frame passed to the signal handler which the
//! signal handler uses to jump back and report failure. Might be somewhat
//! reminiscent of `setjmp`/`longjmp`.
//!
//! # Architecture specifics
//!
//! ## x86_64
//!
//! The details on the parameter passing and the ABI can be found in
//! the System V x86_64 ABI specification, 3.2.3 Parameter Passing.
//!
//! Here is the stack diagram before running the code that may fail:
//!
//! +--------------------+
//! | Previous %rbp      | <- %rbp points here after "mov %rsp, %rbp"
//! +--------------------+
//! | Frame stack pointer| <- frame.stack_pointer, points to the current %rsp - 8 after "push %rsp"
//! +--------------------+
//! | Cookie (-1)        | <- Used to validate the `FailureFrame`
//! +--------------------+
//! | Jump Address       | <- Address to return to on failure
//! +--------------------+
//! | AccessFailure Addr | <- Address of `AccessFailure` struct
//! +--------------------+
//! | Previous %r15      | <- Non-volatile register %r15 saved
//! +--------------------+
//! | Previous %r14      | <- Non-volatile register %r14 saved
//! +--------------------+
//! | Previous %r13      | <- Non-volatile register %r13 saved
//! +--------------------+
//! | Previous %r12      | <- Non-volatile register %r12 saved
//! +--------------------+
//! | Previous %rbp      | <- Non-volatile register %rbp saved
//! +--------------------+
//! | Previous %rbx      | <- Non-volatile register %rbx saved
//! +--------------------+ <- %rsp (stack pointer) initially after "sub $32, %rsp"
//!
//! ## aarch64
//!
//! Calling Conventions and the ABI are governed by the AArch64 Procedure Call Standard
//! (AAPCS64), the OS'es are expected to adhere to that. That said, the implementation
//! of the `try_*` primitves should be the same for Linux, macOS, and Windows.
//!
//! Stack diagram:
//!
//! +--------------------+
//! | FailureFrame SP    | <- Frame stack pointer, points to the current SP
//! +--------------------+
//! | Cookie (-1)        | <- Used to validate the `FailureFrame`
//! +--------------------+
//! | Jump Address       | <- Address to return to on failure
//! +--------------------+
//! | AccessFailure Addr | <- Address of `AccessFailure` struct
//! +--------------------+
//! | Previous x28       | <- Non-volatile register x28 saved
//! +--------------------+
//! | Previous x27       | <- Non-volatile register x27 saved
//! +--------------------+
//! | Previous x26       | <- Non-volatile register x26 saved
//! +--------------------+
//! | Previous x25       | <- Non-volatile register x25 saved
//! +--------------------+
//! | Previous x24       | <- Non-volatile register x24 saved
//! +--------------------+
//! | Previous x23       | <- Non-volatile register x23 saved
//! +--------------------+
//! | Previous x22       | <- Non-volatile register x22 saved
//! +--------------------+
//! | Previous x21       | <- Non-volatile register x21 saved
//! +--------------------+
//! | Previous x20       | <- Non-volatile register x20 saved
//! +--------------------+
//! | Previous x19       | <- Non-volatile register x19 saved
//! +--------------------+ <- SP (stack pointer) initially

#![cfg(any(target_os = "linux", target_os = "macos"))]

use crate::AccessFailure;
use libc::sigaction;
use libc::sigemptyset;
use libc::siginfo_t;
use libc::SA_NODEFER;
use libc::SA_SIGINFO;
use libc::SIGBUS;
use libc::SIGSEGV;
use libc::SIG_DFL;

#[repr(C, align(8))]
struct FailureFrame {
    stack_pointer: u64,
    cookie: u64,
    jump_to: u64,
    access_failure: *mut AccessFailure,
}

/// Restore the default handler and continue to crash the process.
fn restore_sig(sig: i32) {
    // SAFETY: no state is shared between threads, the code runs in one thread.
    unsafe {
        let mut act: sigaction = std::mem::zeroed();
        act.sa_sigaction = SIG_DFL;
        sigemptyset(&mut act.sa_mask);
        sigaction(sig, &act, std::ptr::null_mut());
    }
}

extern "C" fn handle_signal(sig: i32, info: *mut siginfo_t, ucontext: *mut libc::c_void) {
    if (sig != SIGSEGV && sig != SIGBUS) || info.is_null() || ucontext.is_null() {
        restore_sig(sig);
        return;
    }

    // SAFETY: the OS provides valid data, the pointer is not null
    // per the check above.
    let info = unsafe { info.as_ref().unwrap() };
    let ucontext: *const libc::ucontext_t = ucontext.cast();
    // SAFETY: the OS provides valid data, the pointer is not null
    // per the check above.
    let ucontext = unsafe { ucontext.as_ref().unwrap() };

    // xtask-fmt allow-target-arch sys-crate
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    let frame_addr = ucontext.uc_mcontext.gregs[libc::REG_RBP as usize];

    // xtask-fmt allow-target-arch sys-crate
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    let frame_addr = ucontext.uc_mcontext.regs[29];

    // xtask-fmt allow-target-arch sys-crate
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    // SAFETY: the OS provides valid data, the pointer is not null
    // per the check above.
    let frame_addr = unsafe { (*ucontext.uc_mcontext).__ss.__fp };

    // xtask-fmt allow-target-arch sys-crate
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    compile_error!("Not supported");

    // SAFETY: a `try_*` function provides valid data.
    let frame = unsafe { (frame_addr as *const FailureFrame).as_ref().unwrap() };
    debug_assert_eq!(frame.cookie, !0);

    // SAFETY: a `try_*` function provides valid data.
    let access_failure = unsafe { frame.access_failure.as_mut().unwrap() };
    // SAFETY: a `try_*` function provides valid data, the pointer is not null
    // per the check above.
    access_failure.address = unsafe { info.si_addr() }.cast();
    access_failure.si_code = info.si_code;
    access_failure.si_signo = info.si_signo;

    // Jump back.

    // SAFETY: a `try_*` function provides the valid instruction and the stack pointers.
    // xtask-fmt allow-target-arch sys-crate
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::asm!(
            "mov {0}, %rsp",
            "jmpq *{1}",
            in(reg) frame.stack_pointer - 8, // Strip the last 8 bytes after "push %rsp, %rbp"
            in(reg) frame.jump_to,
            options(att_syntax, noreturn));
    }

    // SAFETY: a `try_*` function provides the valid instruction and the stack pointers.
    // xtask-fmt allow-target-arch sys-crate
    #[cfg(target_arch = "aarch64")]
    unsafe {
        std::arch::asm!(
            "mov sp, {0}",
            "br {1}",
            in(reg) frame.stack_pointer,
            in(reg) frame.jump_to,
            options(noreturn));
    }
}

/// Installs signal handlers for SIGSEGV and SIGBUS.
pub fn install_signal_handlers() -> Result<(), i32> {
    // SAFETY: installing signal handlers single-threaded, calling the C
    // functions as the system manual describes.
    unsafe {
        let mut act: sigaction = std::mem::zeroed();
        act.sa_sigaction = handle_signal as usize;
        act.sa_flags = SA_NODEFER | SA_SIGINFO;

        if sigemptyset(&mut act.sa_mask) == -1 {
            #[cfg(target_os = "macos")]
            return Err(*libc::__error());

            #[cfg(target_os = "linux")]
            return Err(*libc::__errno_location());
        }

        let signals = [SIGSEGV, SIGBUS];
        for &sig in &signals {
            if sigaction(sig, &act, std::ptr::null_mut()) == -1 {
                #[cfg(target_os = "macos")]
                return Err(*libc::__error());

                #[cfg(target_os = "linux")]
                return Err(*libc::__errno_location());
            }
        }
    }
    Ok(())
}

// xtask-fmt allow-target-arch sys-crate
#[cfg(target_arch = "x86_64")]
mod x86_64 {
    /// Defines a function that can handle fatal signals. `$head` runs before the try,
    /// `$body` runs inside the try, and `$tail` runs after the try.
    ///
    /// If code faults while running the instructions in `$body`, then the exception
    /// filter will fill out the [`AccessFailure`] pointed to by `$failure_reg`, and
    /// the function will return -1.
    macro_rules! try_op {
        ($func:path, $failure_reg:expr, [$($head:expr),* $(,)?], [$($body:expr),* $(,)?], [$($tail:expr),* $(,)?]) => {
            std::arch::global_asm! {
                ".pushsection .text",
                ".globl {func}",
                ".p2align 4",
                "{func}:",

                "endbr64", // TODO: W/o endbr64 Windows wouldn't boot 100%, with this ~80%
                // Save non-volatiles, signal handling may clobber these
                "push %rbx","push %rbp","push %r12","push %r13","push %r14","push %r15",

                // Helper to get the instruction pointer and avoid relocations
                "jmp 4f",
                "0:",
                "mov (%rsp), %rax",
                "ret",
                "4:",

                // Create `FailureFrame`
                concat!("push ", $failure_reg), // Pass the address of the `AccessFailure` to the signal handler
                // Generate a pointer to the jump from the signal handler
                "call 0b",
                "5:",
                "add $(2f - 5b), %rax",
                "push %rax", // Pointer to jump to if the signal is delivered
                "push $-1", // Pass a cookie
                "push %rsp", // Pass RSP to the signal handler
                "mov %rsp, %rbp",

                "jmp 1f",
                "2:",
                "mov $-1, %eax", // return -1 on failure (32-bit)
                "jmp 3f",

                "1:",
                $($head,)*
                $($body,)*
                $($tail,)*

                "3:",
                "add $32, %rsp",
                // Restore non-volatiles, might've been clobbered by jumping out of signal handling
                "pop %r15","pop %r14","pop %r13","pop %r12","pop %rbp","pop %rbx",
                // Clear the direction flag as req'd by the System-V x86-64 ABI
                "cld",
                "ret",
                ".popsection",
                func = sym $func,
                options(att_syntax),
            }
        };
    }

    try_op!(
        crate::try_memmove,
        "%rcx",
        [],
        ["call memcpy", "endbr64"], // TODO: W/o endbr64 Windows wouldn't boot 100%, with this ~80%
        ["xorl %eax, %eax"]
    );
    try_op!(
        crate::try_memset,
        "%rcx",
        [],
        ["call memset", "endbr64"], // TODO: W/o endbr64 Windows wouldn't boot 100%, with this ~80%
        ["xorl %eax, %eax"]
    );
    try_op!(
        crate::try_cmpxchg8,
        "%rcx",
        ["movb (%rsi), %al"],
        ["cmpxchg %dl, (%rdi)"],
        ["movb %al, (%rsi)", "setz %al", "movzx %al, %eax"]
    );
    try_op!(
        crate::try_cmpxchg16,
        "%rcx",
        ["movw (%rsi), %ax",],
        ["cmpxchg %dx, (%rdi)"],
        ["movw %ax, (%rsi)", "setz %al", "movzx %al, %eax"]
    );
    try_op!(
        crate::try_cmpxchg32,
        "%rcx",
        ["movl (%rsi), %eax",],
        ["cmpxchg %edx, (%rdi)"],
        ["movl %eax, (%rsi)", "setz %al", "movzx %al, %eax"]
    );
    try_op!(
        crate::try_cmpxchg64,
        "%rcx",
        ["movq (%rsi), %rax",],
        ["cmpxchg %rdx, (%rdi)"],
        ["movq %rax, (%rsi)", "setz %al", "movzx %al, %eax"]
    );
    try_op!(
        crate::try_read8,
        "%rdx",
        [],
        ["movb (%rsi), %al"],
        ["movb %al, (%rdi)", "xorl %eax, %eax"]
    );
    try_op!(
        crate::try_read16,
        "%rdx",
        [],
        ["movw (%rsi), %ax"],
        ["movw %ax, (%rdi)", "xorl %eax, %eax"]
    );
    try_op!(
        crate::try_read32,
        "%rdx",
        [],
        ["movl (%rsi), %eax"],
        ["movl %eax, (%rdi)", "xorl %eax, %eax"]
    );
    try_op!(
        crate::try_read64,
        "%rdx",
        [],
        ["movq (%rsi), %rax"],
        ["movq %rax, (%rdi)", "xorl %eax, %eax"]
    );
    try_op!(
        crate::try_write8,
        "%rdx",
        [],
        ["movb %sil, (%rdi)"],
        ["xorl %eax, %eax"]
    );
    try_op!(
        crate::try_write16,
        "%rdx",
        [],
        ["movw %si, (%rdi)"],
        ["xorl %eax, %eax"]
    );
    try_op!(
        crate::try_write32,
        "%rdx",
        [],
        ["movl %esi, (%rdi)"],
        ["xorl %eax, %eax"]
    );
    try_op!(
        crate::try_write64,
        "%rdx",
        [],
        ["movq %rsi, (%rdi)"],
        ["xorl %eax, %eax"]
    );
}

// xtask-fmt allow-target-arch sys-crate
#[cfg(target_arch = "aarch64")]
mod aarch64 {
    /// Defines a function that can handle fatal signals. `$head` runs before the try,
    /// `$body` runs inside the try, and `$tail` runs after the try.
    ///
    /// If code faults while running the instructions in `$body`, then the exception
    /// filter will fill out the [`AccessFailure`] pointed to by `$failure_reg`, and
    /// the function will return -1.
    macro_rules! try_proc {
        ($func:path, $failure_reg:expr, [$($head:expr),* $(,)?], [$($body:expr),* $(,)?], [$($tail:expr),* $(,)?]) => {
            std::arch::global_asm! {
                ".globl {func}",
                ".arch armv8.1-a",
                ".p2align 2",
                "{func}:",

                // Save non-volatile registers
                "stp x19, x20, [sp, #-16]!",
                "stp x21, x22, [sp, #-16]!",
                "stp x23, x24, [sp, #-16]!",
                "stp x25, x26, [sp, #-16]!",
                "stp x27, x28, [sp, #-16]!",
                "stp x29, x30, [sp, #-16]!",

                // Create `FailureFrame`
                "sub sp, sp, #32",
                "mov x29, sp", // Set the frame pointer for the signal handler
                "str x29, [sp]", // Store the stack pointer
                "mov x19, #-1", // Cookie value
                "str x19, [sp, #8]", // Store the cookie
                "adr x19, 3f", // Generate a pointer to the jump from the signal handler
                "str x19, [sp, #16]", // Store the jump address
                concat!("str ", $failure_reg, ", [sp, #24]"), // Pass the address of the `AccessFailure` to the signal handler

                $($head,)*
                $($body,)*
                $($tail,)*

                "2:",
                // Deallocate `FailureFrame`
                "add sp, sp, #32",

                // Restore non-volatile registers
                "ldp x29, x30, [sp], #16",
                "ldp x27, x28, [sp], #16",
                "ldp x25, x26, [sp], #16",
                "ldp x23, x24, [sp], #16",
                "ldp x21, x22, [sp], #16",
                "ldp x19, x20, [sp], #16",
                "ret",

                "3:",
                "mov w0, #-1", // return -1 on failure
                "b 2b",

                func = sym $func,
            }
        };
    }

    #[cfg(target_os = "macos")]
    try_proc!(
        crate::try_memmove,
        "x3",
        [],
        ["bl _memcpy"],
        ["mov w0, wzr"]
    );
    #[cfg(target_os = "macos")]
    try_proc!(crate::try_memset, "x3", [], ["bl _memset"], ["mov w0, wzr"]);
    #[cfg(not(target_os = "macos"))]
    try_proc!(crate::try_memmove, "x3", [], ["bl memcpy"], ["mov w0, wzr"]);
    #[cfg(not(target_os = "macos"))]
    try_proc!(crate::try_memset, "x3", [], ["bl memset"], ["mov w0, wzr"]);
    try_proc!(
        crate::try_cmpxchg8,
        "x3",
        ["ldrb w8, [x1]", "mov w9, w8"],
        ["casalb w8, w2, [x0]"],
        ["strb w8, [x1]", "cmp w8, w9", "cset w0, eq"]
    );
    try_proc!(
        crate::try_cmpxchg16,
        "x3",
        ["ldrh w8, [x1]", "mov w9, w8"],
        ["casalh w8, w2, [x0]"],
        ["strh w8, [x1]", "cmp w8, w9", "cset w0, eq"]
    );
    try_proc!(
        crate::try_cmpxchg32,
        "x3",
        ["ldr w8, [x1]", "mov w9, w8"],
        ["casal w8, w2, [x0]"],
        ["str w8, [x1]", "cmp w8, w9", "cset w0, eq"]
    );
    try_proc!(
        crate::try_cmpxchg64,
        "x3",
        ["ldr x8, [x1]", "mov x9, x8"],
        ["casal x8, x2, [x0]"],
        ["str x8, [x1]", "cmp x8, x9", "cset w0, eq"]
    );
    try_proc!(
        crate::try_read8,
        "x2",
        [],
        ["ldrb w8, [x1]"],
        ["strb w8, [x0]", "mov w0, wzr"]
    );
    try_proc!(
        crate::try_read16,
        "x2",
        [],
        ["ldrh w8, [x1]"],
        ["strh w8, [x0]", "mov w0, wzr"]
    );
    try_proc!(
        crate::try_read32,
        "x2",
        [],
        ["ldr w8, [x1]"],
        ["str w8, [x0]", "mov w0, wzr"]
    );
    try_proc!(
        crate::try_read64,
        "x2",
        [],
        ["ldr x8, [x1]"],
        ["str x8, [x0]", "mov w0, wzr"]
    );
    try_proc!(
        crate::try_write8,
        "x2",
        [],
        ["strb w1, [x0]"],
        ["mov w0, wzr"]
    );
    try_proc!(
        crate::try_write16,
        "x2",
        [],
        ["strh w1, [x0]"],
        ["mov w0, wzr"]
    );
    try_proc!(
        crate::try_write32,
        "x2",
        [],
        ["str w1, [x0]"],
        ["mov w0, wzr"]
    );
    try_proc!(
        crate::try_write64,
        "x2",
        [],
        ["str x1, [x0]"],
        ["mov w0, wzr"]
    );
}
