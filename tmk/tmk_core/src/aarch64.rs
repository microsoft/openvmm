// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Aarch64 entry point and support.

#![cfg(target_arch = "aarch64")]

use super::Scope;

#[cfg(minimal_rt)]
mod entry {
    core::arch::global_asm! {
        ".weak _DYNAMIC",
        ".hidden _DYNAMIC",
        ".globl _start",

        // Good only for the EL1
        " __exception_common:",

        "str     x29, [sp, #-16]!",
        "stp     x27, x28, [sp, #-16]!",
        "stp     x25, x26, [sp, #-16]!",
        "stp     x23, x24, [sp, #-16]!",
        "stp     x21, x22, [sp, #-16]!",
        "stp     x19, x20, [sp, #-16]!",
        "stp     x17, x18, [sp, #-16]!",
        "stp     x15, x16, [sp, #-16]!",
        "stp     x13, x14, [sp, #-16]!",
        "stp     x11, x12, [sp, #-16]!",
        "stp     x9, x10, [sp, #-16]!",
        "stp     x7, x8, [sp, #-16]!",
        "stp     x5, x6, [sp, #-16]!",
        "stp     x3, x4, [sp, #-16]!",
        "stp     x1, x2, [sp, #-16]!",

        "add     sp, sp, #-16",

        "mrs     x2, spsr_el1",
        "mrs     x1, elr_el1",
        "stp     x1, x2, [sp, #-16]!",

        "str     x0, [sp, #-16]!",

        "mrs     x2, tpidr_el1",
        "add     x1, sp, #38*8",
        "stp     x1, x2, [sp, #32]",

        "mov     x0, sp",
        "bl      exception_handler",

        "ldr     x1, [sp, #40]",
        "msr     tpidr_el1, x1",

        "add     sp, sp, #16",

        "ldp     x1, x2, [sp], #16",
        "msr     elr_el1, x1",
        "msr     spsr_el1, x2",

        "add     sp, sp, #16",

        "ldp     x1, x2, [sp], #16",
        "ldp     x3, x4, [sp], #16",
        "ldp     x5, x6, [sp], #16",
        "ldp     x7, x8, [sp], #16",
        "ldp     x9, x10, [sp], #16",
        "ldp     x11, x12, [sp], #16",
        "ldp     x13, x14, [sp], #16",
        "ldp     x15, x16, [sp], #16",
        "ldp     x17, x18, [sp], #16",
        "ldp     x19, x20, [sp], #16",
        "ldp     x21, x22, [sp], #16",
        "ldp     x23, x24, [sp], #16",
        "ldp     x25, x26, [sp], #16",
        "ldp     x27, x28, [sp], #16",
        "ldr     x29, [sp], #16",
        "ldp     lr, x0, [sp], #16",

        "eret",

        ".macro EXCEPTION_ENTRY source, kind",
        ".align 7",
        "	stp     lr, x0, [sp, #-16]!",
        "	mov     x0, \\source",
        "	movk    x0, \\kind, lsl #16",
        "	b       __exception_common",
        ".endm",

        // Vector table must be aligned to a 2KB boundary
        ".balign 0x800",
        " _vector_table_el1:",
        // Target and source at same exception level with source SP = SP_EL0
        "     EXCEPTION_ENTRY #0x0, #0x0",  // Synchronous exception
        "     EXCEPTION_ENTRY #0x0, #0x1",  // IRQ
        "     EXCEPTION_ENTRY #0x0, #0x2",  // FIQ
        "     EXCEPTION_ENTRY #0x0, #0x3",  // SError
        // Target and source at same exception level with source SP = SP_ELx
        "     EXCEPTION_ENTRY #0x1, #0x0  // Synchronous exception",
        "     EXCEPTION_ENTRY #0x1, #0x1  // IRQ",
        "     EXCEPTION_ENTRY #0x1, #0x2  // FIQ",
        "     EXCEPTION_ENTRY #0x1, #0x3  // SError",
        // Source is at lower exception level running on AArch64
        "     EXCEPTION_ENTRY #0x2, #0x0",  // Synchronous exception
        "     EXCEPTION_ENTRY #0x2, #0x1",  // IRQ
        "     EXCEPTION_ENTRY #0x2, #0x2",  // FIQ
        "     EXCEPTION_ENTRY #0x2, #0x3",  // SError
        // Source is at lower exception level running on AArch32
        "     EXCEPTION_ENTRY #0x3, #0x0",  // Synchronous exception
        "     EXCEPTION_ENTRY #0x3, #0x1",  // IRQ
        "     EXCEPTION_ENTRY #0x3, #0x2",  // FIQ
        "     EXCEPTION_ENTRY #0x3, #0x3",  // SError

        "_start:",
        "mov x19, x0",
        "adrp x1, {stack}",
        "add x1, x1, :lo12:{stack}",
        "add x1, x1, {STACK_SIZE}",
        "mov sp, x1",

        // Enable the FPU.
        "mrs     x0, CPACR_EL1",
        "orr     x0, x0, #(3 << 20)",
        "orr     x0, x0, #(3 << 16)",
        "msr     CPACR_EL1, x0",
        "isb",

        // Set up the vector table.
        "adrp   x3, _vector_table_el1",
        "add    x3, x3, :lo12:_vector_table_el1",
        "msr    VBAR_EL1, x3",
        "isb",

        "adrp x0, __ehdr_start",
        "add x0, x0, :lo12:__ehdr_start",
        "mov x1, x0",
        "adrp x2, _DYNAMIC",
        "add x2, x2, :lo12:_DYNAMIC",
        "bl {relocate}",
        "mov x0, x19",
        "b {entry}",

        relocate = sym minimal_rt::reloc::relocate,
        stack = sym STACK,
        entry = sym crate::entry,
        STACK_SIZE = const STACK_SIZE,
    }

    const STACK_SIZE: usize = 16384;
    #[repr(C, align(16))]
    struct Stack([u8; STACK_SIZE]);
    static mut STACK: Stack = Stack([0; STACK_SIZE]);
}

pub(super) struct ArchScopeState;

impl Scope<'_, '_> {
    pub(super) fn arch_init() -> ArchScopeState {
        ArchScopeState
    }
    pub(super) fn arch_reset(&mut self) {}
}

#[unsafe(no_mangle)]
extern "C" fn exception_handler(_exception_frame: *const ()) {}
