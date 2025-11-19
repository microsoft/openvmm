// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86_64 intrinsics.

/// Causes a processor fault.
pub fn fault() -> ! {
    // SAFETY: ud2 is always safe, and will cause the function to diverge.
    unsafe {
        core::arch::asm!("ud2");
        core::hint::unreachable_unchecked()
    }
}

/// Spins forever, preserving some context in the registers.
pub fn dead_loop(code0: u64, code1: u64, code2: u64) -> ! {
    // SAFETY: This spin loop has no safety conditions.
    unsafe {
        core::arch::asm!("1: jmp 1b", in ("rdi") code0, in ("rsi") code1, in ("rax") code2, options(att_syntax));
        core::hint::unreachable_unchecked()
    }
}
