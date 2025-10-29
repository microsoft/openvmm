// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Rust implementation of `memcpy` and `memmove`. Useful when the system
//! `memcpy` is slow (e.g., musl x86_64 on some CPUs).

// UNSAFETY: implementing low-level memory functions.
#![expect(unsafe_code)]
// Safety docs don't seem useful here.
#![allow(clippy::missing_safety_doc)]
#![allow(clippy::undocumented_unsafe_blocks)]

/// Optimized memmove implementation.
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn memmove(dest: *mut u8, src: *const u8, len: usize) -> *mut u8 {
    // Our memcpy handles overlapping regions correctly.
    unsafe { memcpy(dest, src, len) }
}

/// Optimized memcpy implementation.
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, len: usize) -> *mut u8 {
    unsafe {
        // Handle small sizes with specialized code. For some values, perform a
        // single read+write of the appropriate size. For others, read+write
        // potentially overlapping head and tail values to cover the entire
        // range.
        match len {
            0 => {}
            1 => copy_one::<u8>(dest, src),
            2 => copy_one::<u16>(dest.cast(), src.cast()),
            3 => copy_one::<[u8; 3]>(dest.cast(), src.cast()),
            4 => copy_one::<u32>(dest.cast(), src.cast()),
            n if n < 8 => copy_two::<u32>(dest.cast(), src.cast(), len),
            n if n < 16 => copy_two::<u64>(dest.cast(), src.cast(), len),
            n if n <= 32 => copy_two::<u128>(dest.cast(), src.cast(), len),
            n if n <= 64 => copy_two::<[u128; 2]>(dest.cast(), src.cast(), len),
            n if n <= 128 => copy_two::<[u128; 4]>(dest.cast(), src.cast(), len),
            _ => {
                // This is a big copy. Align `dest` so that writes, at least,
                // are aligned. Then loop using 64-byte chunks, which gives the
                // compiler some room to optimize.
                if !overlaps(dest, src, len) {
                    // Copy the first 16 bytes, then resume at the next aligned
                    // address.
                    copy_one::<u128>(dest.cast(), src.cast());
                    let offset = 16 - dest.addr() % 16;
                    copy_loop_dest_aligned_forward::<[u128; 4]>(
                        dest.byte_add(offset).cast(),
                        src.byte_add(offset).cast(),
                        len - offset,
                    );
                } else if dest.addr() <= src.addr() {
                    // Save the first 16 bytes, writing them after the rest is
                    // copied in the forward direction to avoid overwriting what
                    // we're reading.
                    let head = src.cast::<u128>().read_unaligned();
                    let offset = 16 - dest.addr() % 16;
                    copy_loop_dest_aligned_forward::<[u128; 4]>(
                        dest.byte_add(offset).cast(),
                        src.byte_add(offset).cast(),
                        len - offset,
                    );
                    // Write the head now that the rest is copied.
                    dest.cast::<u128>().write_unaligned(head);
                } else {
                    // As before, but save the _last_ 16 bytes and copy
                    // backwards to avoid overwriting what we're reading.
                    let tail = src.byte_add(len - 16).cast::<u128>().read_unaligned();
                    let offset = (dest.addr() + len) % 16;
                    copy_loop_dest_aligned_backward::<[u128; 4]>(
                        dest.cast(),
                        src.cast(),
                        len - offset,
                    );
                    // Write the tail now that the rest is copied.
                    dest.byte_add(len - 16).cast::<u128>().write_unaligned(tail);
                }
            }
        }
    }
    dest
}

fn overlaps(dest: *mut u8, src: *const u8, len: usize) -> bool {
    dest.addr().abs_diff(src.addr()) < len
}

/// Copies one element of size `T` from `src` to `dest`.
///
/// Alignment not required. Overlap is allowed.
unsafe fn copy_one<T>(dest: *mut T, src: *const T) {
    unsafe { dest.write_unaligned(src.read_unaligned()) };
}

/// Copies the beginning and ending `T`s from `[src..src+len]` to
/// `[dest..dest+len]`.
///
/// Alignment is not required. Overlap is allowed.
///
/// The intended use of this is when `len <= 2 * size_of::<T>()`, so that the
/// two copies cover the entire range.
unsafe fn copy_two<T>(dest: *mut T, src: *const T, len: usize) {
    unsafe {
        // Read both ends first in case of overlap.
        let a = src.read_unaligned();
        let b = src.byte_add(len - size_of::<T>()).read_unaligned();
        dest.write_unaligned(a);
        dest.byte_add(len - size_of::<T>()).write_unaligned(b);
    }
}

/// Copies `[src..src+len]` to `[dest..dest+len]` using copies of size `T`.
///
/// `dest` must be aligned, and `len` must be at least `size_of::<T>()`.
///
/// Overlap is allowed, but the copy is done forwards, so `dest` must be
/// before `src` or non-overlapping.
unsafe fn copy_loop_dest_aligned_forward<T>(dest: *mut T, src: *const T, len: usize) {
    unsafe {
        debug_assert!(dest.is_aligned());
        debug_assert!(!overlaps(dest.cast(), src.cast(), len) || dest.addr() <= src.addr());
        debug_assert!(len >= size_of::<T>());

        // Save the tail now in case it is overlapping.
        let tail = src.byte_add(len - size_of::<T>()).read_unaligned();
        // Copy until the last chunk.
        let mut i = 0;
        loop {
            dest.byte_add(i).write(src.byte_add(i).read_unaligned());
            i += size_of::<T>();
            if i >= len - size_of::<T>() {
                break;
            }
        }
        // Write the tail.
        dest.byte_add(len - size_of::<T>()).write_unaligned(tail);
    }
}

/// Copies `[src..src+len]` to `[dest..dest+len]` using copies of size `T`,
/// backwards.
///
/// `dest+len` must be aligned, and `len` must be at least `size_of::<T>()`.
///
/// Overlap is allowed, but the copy is done backwards, so `dest` must be after
/// `src` or non-overlapping.
unsafe fn copy_loop_dest_aligned_backward<T>(dest: *mut T, src: *const T, len: usize) {
    unsafe {
        debug_assert!(dest.byte_add(len).is_aligned());
        debug_assert!(!overlaps(dest.cast(), src.cast(), len) || dest.addr() >= src.addr());
        debug_assert!(len >= size_of::<T>());

        // Save the head now in case it is overlapping.
        let head = src.read_unaligned();
        // Copy until the last chunk.
        let mut i = len - size_of::<T>();
        loop {
            dest.byte_add(i).write(src.byte_add(i).read_unaligned());
            if i <= size_of::<T>() {
                break;
            }
            i -= size_of::<T>();
        }
        // Write the head.
        dest.write_unaligned(head);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_memcpy() {
        let max = 8000;
        let src = (0..max).map(|x| (x % 256) as u8).collect::<Vec<u8>>();
        let mut dest = vec![0u8; max];
        for i in 0..max {
            let dest = &mut dest[max - i..];
            let src = &src[max - i..];
            dest.fill(0);
            unsafe {
                super::memcpy(
                    core::hint::black_box(dest.as_mut_ptr()),
                    core::hint::black_box(src.as_ptr()),
                    core::hint::black_box(i),
                )
            };
            assert_eq!(dest, src);
        }
    }

    #[test]
    fn test_memmove() {
        let data = (0..16000).map(|x| (x % 256) as u8).collect::<Vec<u8>>();
        for len in [
            0, 1, 2, 3, 4, 5, 8, 13, 21, 34, 55, 64, 89, 128, 144, 233, 256, 377, 512, 610, 987,
            1597,
        ] {
            for offset in -1024..1024 {
                let mut buf = data.clone();
                let src_ptr = buf.as_ptr().wrapping_add(8000);
                let dest_ptr = buf.as_mut_ptr().wrapping_offset(8000 + offset);
                let expected = {
                    let mut expected = buf.clone();
                    expected.copy_within(8000..8000 + len, (8000 + offset) as usize);
                    expected
                };
                unsafe {
                    super::memcpy(
                        core::hint::black_box(dest_ptr),
                        core::hint::black_box(src_ptr),
                        core::hint::black_box(len),
                    )
                };
                assert_eq!(buf, expected, "len={}, offset={}", len, offset);
            }
        }
    }
}
