// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Architecture-independent runtime support.

#[cfg(minimal_rt)]
#[expect(clippy::missing_safety_doc)]
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

    /// Hand rolled implementation of memcpy.
    // SAFETY: The minimal_rt_build crate ensures that when this code is compiled
    // there is no libc for this to conflict with.
    #[unsafe(no_mangle)]
    unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, len: usize) -> *mut u8 {
        for i in 0..len {
            // SAFETY: the caller guarantees the pointer and length are correct.
            unsafe { core::ptr::write(dest.add(i), core::ptr::read(src.add(i))) };
        }

        dest
    }

    /// Hand rolled implementation of memset.
    // SAFETY: The minimal_rt_build crate ensures that when this code is compiled
    // there is no libc for this to conflict with.
    #[unsafe(no_mangle)]
    unsafe extern "C" fn memset(ptr: *mut u8, val: i32, len: usize) -> *mut u8 {
        for i in 0..len {
            // SAFETY: the caller guarantees the pointer and length are correct.
            unsafe { core::ptr::write(ptr.add(i), val as u8) };
        }

        ptr
    }

    /// Implementation cribbed from compiler_builtins.
    // SAFETY: The minimal_rt_build crate ensures that when this code is compiled
    // there is no libc for this to conflict with.
    #[unsafe(no_mangle)]
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
    // SAFETY: The minimal_rt_build crate ensures that when this code is compiled
    // there is no libc for this to conflict with.
    #[unsafe(no_mangle)]
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
