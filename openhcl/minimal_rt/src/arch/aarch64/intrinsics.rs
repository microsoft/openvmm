// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! aarch64 intrinsics.

#![cfg_attr(minimal_rt, expect(clippy::missing_safety_doc))]

#[cfg(minimal_rt)]
// SAFETY: The minimal_rt_build crate ensures that when this code is compiled
// there is no libc for this to conflict with.
#[unsafe(no_mangle)]
pub static __stack_chk_guard: usize = 0x0BADC0DEDEADBEEF;

#[cfg(minimal_rt)]
// SAFETY: The minimal_rt_build crate ensures that when this code is compiled
// there is no libc for this to conflict with.
#[unsafe(no_mangle)]
unsafe extern "C" fn __stack_chk_fail() {
    panic!("stack smashing detected");
}

/// Causes a processor fault.
#[inline(always)]
pub fn fault() -> ! {
    // SAFETY: faults the processor, so the program ends.
    unsafe {
        core::arch::asm!("brk #0");
        core::hint::unreachable_unchecked()
    }
}

/// Spins forever, preserving some context in the registers.
#[inline(always)]
pub fn dead_loop(code0: u64, code1: u64, code2: u64) -> ! {
    // SAFETY: no safety requirements.
    unsafe {
        core::arch::asm!("b .", in ("x0") code0, in ("x1") code1, in ("x2") code2);
        core::hint::unreachable_unchecked()
    }
}
