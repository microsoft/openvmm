// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Rust implementation of `memcpy` and `memmove`. Useful when the system
//! `memcpy` is slow (e.g., musl x86_64 on some CPUs).

// UNSAFETY: implementing low-level memory functions.
#![expect(unsafe_code)]
// Skip the noise of safety docs, since every function has essentially the same
// contract and safety justifications.
#![expect(clippy::missing_safety_doc)]
#![expect(clippy::undocumented_unsafe_blocks)]

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
                    let head = read_one(src.cast::<u128>());
                    let offset = 16 - dest.addr() % 16;
                    copy_loop_dest_aligned_forward::<[u128; 4]>(
                        dest.byte_add(offset).cast(),
                        src.byte_add(offset).cast(),
                        len - offset,
                    );
                    // Write the head now that the rest is copied.
                    write_one(dest.cast::<u128>(), head);
                } else {
                    // As before, but save the _last_ 16 bytes and copy
                    // backwards to avoid overwriting what we're reading.
                    let tail = read_one(src.byte_add(len - 16).cast::<u128>());
                    let offset = (dest.addr() + len) % 16;
                    copy_loop_dest_aligned_backward::<[u128; 4]>(
                        dest.cast(),
                        src.cast(),
                        len - offset,
                    );
                    // Write the tail now that the rest is copied.
                    write_one(dest.byte_add(len - 16).cast::<u128>(), tail);
                }
            }
        }
    }
    dest
}

fn overlaps(dest: *mut u8, src: *const u8, len: usize) -> bool {
    dest.addr().abs_diff(src.addr()) < len
}

// Define methods for reading/writing unaligned chunks of various sizes.
//
// For large chunks, this gets better codegen than using `ptr::read_unaligned`
// directly. In particular, it prevents the compiler from spilling values to the
// stack.
trait Chunk: Copy {
    unsafe fn read_unaligned(this: *const Self) -> Self;
    unsafe fn write_unaligned(this: *mut Self, val: Self);
    unsafe fn write_aligned(this: *mut Self, val: Self);
}

macro_rules! scalar {
    ($($ty:ty),* $(,)?) => {
        $(
        impl Chunk for $ty {
            unsafe fn read_unaligned(this: *const Self) -> Self {
                unsafe { this.read_unaligned() }
            }
            unsafe fn write_unaligned(this: *mut Self, val: Self) {
                unsafe { this.write_unaligned(val) }
            }
            unsafe fn write_aligned(this: *mut Self, val: Self) {
                unsafe { this.write(val) }
            }
        }
        )*
    }
}

scalar!(u8, u16, u32, u64, u128);

impl<T: Chunk, const N: usize> Chunk for [T; N] {
    unsafe fn read_unaligned(this: *const Self) -> Self {
        unsafe { this.read_unaligned() }
    }
    unsafe fn write_unaligned(this: *mut Self, val: Self) {
        let this = this.cast::<T>();
        #[expect(clippy::needless_range_loop, reason = "better codegen")]
        for i in 0..N {
            unsafe { write_one(this.add(i), val[i]) };
        }
    }
    unsafe fn write_aligned(this: *mut Self, val: Self) {
        let this = this.cast::<T>();
        #[expect(clippy::needless_range_loop, reason = "better codegen")]
        for i in 0..N {
            unsafe { write_one_aligned(this.add(i), val[i]) };
        }
    }
}

unsafe fn write_one<T: Chunk>(dest: *mut T, val: T) {
    unsafe { Chunk::write_unaligned(dest, val) };
}

unsafe fn write_one_aligned<T: Chunk>(dest: *mut T, val: T) {
    unsafe { Chunk::write_aligned(dest, val) };
}

unsafe fn read_one<T: Chunk>(src: *const T) -> T {
    unsafe { Chunk::read_unaligned(src) }
}

/// Copies one element of size `T` from `src` to `dest`.
///
/// Alignment not required. Overlap is allowed.
#[inline(always)]
unsafe fn copy_one<T: Chunk>(dest: *mut T, src: *const T) {
    unsafe { write_one(dest, read_one(src)) };
}

/// Copies the beginning and ending `T`s from `[src..src+len]` to
/// `[dest..dest+len]`.
///
/// Alignment is not required. Overlap is allowed.
///
/// The intended use of this is when `len <= 2 * size_of::<T>()`, so that the
/// two copies cover the entire range.
#[inline(always)]
unsafe fn copy_two<T: Chunk>(dest: *mut T, src: *const T, len: usize) {
    unsafe {
        // Read both ends first in case of overlap.
        let a = read_one(src);
        let b = read_one(src.byte_add(len - size_of::<T>()));
        write_one(dest, a);
        write_one(dest.byte_add(len - size_of::<T>()), b);
    }
}

/// Copies `[src..src+len]` to `[dest..dest+len]` using copies of size `T`.
///
/// `dest` must be aligned, and `len` must be at least `size_of::<T>()`.
///
/// Overlap is allowed, but the copy is done forwards, so `dest` must be
/// before `src` or non-overlapping.
unsafe fn copy_loop_dest_aligned_forward<T: Chunk>(dest: *mut T, src: *const T, len: usize) {
    unsafe {
        debug_assert!(dest.is_aligned());
        debug_assert!(!overlaps(dest.cast(), src.cast(), len) || dest.addr() <= src.addr());
        debug_assert!(len >= size_of::<T>());

        // Save the tail now in case it is overlapping.
        let tail = read_one(src.byte_add(len - size_of::<T>()));
        // Copy until the last chunk.
        let mut i = 0;
        loop {
            write_one_aligned(dest.byte_add(i), read_one(src.byte_add(i)));
            i += size_of::<T>();
            if i >= len - size_of::<T>() {
                break;
            }
        }
        // Write the tail.
        write_one(dest.byte_add(len - size_of::<T>()), tail);
    }
}

/// Copies `[src..src+len]` to `[dest..dest+len]` using copies of size `T`,
/// backwards.
///
/// `dest+len` must be aligned, and `len` must be at least `size_of::<T>()`.
///
/// Overlap is allowed, but the copy is done backwards, so `dest` must be after
/// `src` or non-overlapping.
unsafe fn copy_loop_dest_aligned_backward<T: Chunk>(dest: *mut T, src: *const T, len: usize) {
    unsafe {
        debug_assert!(dest.byte_add(len).is_aligned());
        debug_assert!(!overlaps(dest.cast(), src.cast(), len) || dest.addr() >= src.addr());
        debug_assert!(len >= size_of::<T>());

        // Save the head now in case it is overlapping.
        let head = read_one(src);
        // Copy until the last chunk.
        let mut i = len - size_of::<T>();
        loop {
            write_one_aligned(dest.byte_add(i), read_one(src.byte_add(i)));
            if i <= size_of::<T>() {
                break;
            }
            i -= size_of::<T>();
        }
        // Write the head.
        write_one(dest, head);
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
        let data = (0..16000).map(|x| (x * 7) as u8).collect::<Vec<u8>>();
        for len in [
            0, 1, 2, 3, 4, 5, 8, 13, 21, 34, 55, 64, 89, 128, 144, 233, 256, 377, 512, 610, 987,
            1597,
        ] {
            for offset in -1024..1024 {
                let mut buf = data.clone();
                let src_ptr = buf.as_ptr().wrapping_add(8000);
                let dest_ptr = buf.as_mut_ptr().wrapping_offset(8000 + offset);
                let expected = {
                    let mut expected = data.clone();
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
