// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86_64 entry point and support.

#![cfg(target_arch = "x86_64")]

#[cfg(minimal_rt)]
mod entry {
    core::arch::global_asm! {
        ".globl _start",
        "_start:",
        "lea rsp, {STACK_SIZE} + {stack}[rip]",
        "lea rdx, _DYNAMIC[rip]",
        "lea rdi, __ehdr_start[rip]",
        "mov rsi, rdi",
        "call {relocate}",
        "jmp {main}",
        relocate = sym minimal_rt::reloc::relocate,
        stack = sym STACK,
        STACK_SIZE = const STACK_SIZE,
        main = sym crate::tmk::main,
    }

    const STACK_SIZE: usize = 16384;
    #[repr(C, align(16))]
    struct Stack([u8; STACK_SIZE]);
    static mut STACK: Stack = Stack([0; STACK_SIZE]);
}
