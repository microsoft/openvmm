// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! aarch64 intrinsics.

#![cfg_attr(minimal_rt, expect(clippy::missing_safety_doc))]

/// Hand rolled implementation of memcpy.
#[cfg(minimal_rt)]
// SAFETY: The minimal_rt_build crate ensures that when this code is compiled
// there is no libc for this to conflict with.
#[unsafe(no_mangle)]
unsafe extern "C" fn memcpy(mut dest: *mut u8, src: *const u8, len: usize) -> *mut u8 {
    // SAFETY: the caller guarantees the pointers and length are correct.
    unsafe {
        core::arch::asm!(r#"
        mov     x3, xzr
1:
        cmp     x2, x3
        beq     2f
        ldrb    w4, [x1,x3]
        strb    w4, [x0,x3]
        add     x3, x3, 1
        b       1b
2:
        add     x0, x0, x2
    "#,
        inout("x0") dest,
        in("x1") src,
        in("x2") len,
        );
    }
    dest
}

/// Hand rolled implementation of memset.
#[cfg(minimal_rt)]
// SAFETY: The minimal_rt_build crate ensures that when this code is compiled
// there is no libc for this to conflict with.
#[unsafe(no_mangle)]
unsafe extern "C" fn memset(mut ptr: *mut u8, val: i32, len: usize) -> *mut u8 {
    // SAFETY: the caller guarantees the pointer and length are correct.
    unsafe {
        core::arch::asm!(r#"
        mov     x3, xzr
1:
        cmp     x2, x3
        beq     2f
        strb    w1, [x0,x3]
        add     x3, x3, 1
        b       1b
        add     x0, x0, x2
2:
        "#,
        inout("x0") ptr,
        in("x1") val,
        in("x2") len);
    }
    ptr
}

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
