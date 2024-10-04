// Copyright (C) Microsoft Corporation. All rights reserved.

//! Architecture-independent runtime support.

#[cfg(minimal_rt)]
mod instead_of_builtins {
    /// Implementation cribbed from compiler_builtins.
    #[inline(always)]
    unsafe fn copy_backward_bytes(mut dest: *mut u8, mut src: *const u8, n: usize) {
        // SAFETY: The caller guarantees that the pointers and length are correct.
        unsafe {
            let dest_start = dest.sub(n);
            while dest_start < dest {
                dest = dest.sub(1);
                src = src.sub(1);
                *dest = *src;
            }
        }
    }

    extern "C" {
        fn memcpy(dest: *mut u8, src: *const u8, n: usize) -> *mut u8;
    }

    /// Implementation cribbed from compiler_builtins.
    #[no_mangle]
    unsafe extern "C" fn memmove(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
        let delta = (dest as usize).wrapping_sub(src as usize);
        if delta >= n {
            // SAFETY: We can copy forwards because either dest is far enough ahead of src,
            // or src is ahead of dest (and delta overflowed).
            unsafe {
                memcpy(dest, src, n);
            }
        } else {
            // SAFETY: dest and src must be copied backward due to src and dest.
            unsafe {
                let dest = dest.add(n);
                let src = src.add(n);
                copy_backward_bytes(dest, src, n);
            }
        }
        dest
    }

    /// This implementation is cribbed from compiler_builtins. It would be nice to
    /// use those implementation for all the above functions, but those require
    /// nightly as these are not yet stabilized.
    #[no_mangle]
    unsafe extern "C" fn bcmp(s1: *const u8, s2: *const u8, n: usize) -> i32 {
        // SAFETY: The caller guarantees that the pointers and length are correct.
        unsafe {
            let mut i = 0;
            while i < n {
                let a = *s1.add(i);
                let b = *s2.add(i);
                if a != b {
                    return a as i32 - b as i32;
                }
                i += 1;
            }
            0
        }
    }
}
