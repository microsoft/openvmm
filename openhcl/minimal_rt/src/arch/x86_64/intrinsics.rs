// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86_64 intrinsics.

/// Hand rolled implementation of memset.
#[cfg(minimal_rt)]
// SAFETY: The minimal_rt_build crate ensures that when this code is compiled
// there is no libc for this to conflict with.
#[unsafe(no_mangle)]
unsafe extern "C" fn memset(mut ptr: *mut u8, val: i32, len: usize) -> *mut u8 {
    // SAFETY: The caller guarantees that the pointer and length are correct.
    unsafe {
        core::arch::asm!(r#"
            cld
            rep stosb
            "#,
            in("rax") val,
            in("rcx") len,
            inout("rdi") ptr);
    }
    ptr
}

/// Hand rolled implementation of memcpy.
#[cfg(minimal_rt)]
// SAFETY: The minimal_rt_build crate ensures that when this code is compiled
// there is no libc for this to conflict with.
#[unsafe(no_mangle)]
unsafe extern "C" fn memcpy(mut dest: *mut u8, src: *const u8, len: usize) -> *mut u8 {
    // SAFETY: The caller guarantees that the pointers and length are correct.
    unsafe {
        core::arch::asm!(r#"
            cld
            rep movsb
            "#,
            in("rsi") src,
            in("rcx") len,
            inout("rdi") dest);
    }
    dest
}

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
