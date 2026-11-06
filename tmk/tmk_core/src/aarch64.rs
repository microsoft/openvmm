// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Aarch64 entry point and support.

#![cfg(target_arch = "aarch64")]

use super::Scope;

#[cfg(minimal_rt)]
mod entry {
    /// # Safety
    /// Must be entered with the TMK start input in X0 and the image identity mapped.
    #[unsafe(no_mangle)]
    #[unsafe(naked)]
    unsafe extern "C" fn _start() -> ! {
        core::arch::naked_asm!(
            ".extern _DYNAMIC",
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
        );
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
