// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![no_std]
// UNSAFETY: Manual pointer manipulation and transmutes to/from atomic types.
#![expect(unsafe_code)]
#![expect(clippy::undocumented_unsafe_blocks)]

use core::mem;
use core::sync::atomic;
use core::sync::atomic::AtomicU8;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// A helper trait for types that can be safely transmuted to and from byte
/// slices.
pub trait AsAtomicBytes: IntoBytes + FromBytes + Immutable + KnownLayout {
    /// Casts the type to a slice of atomic bytes.
    fn as_atomic_bytes(&mut self) -> &[AtomicU8] {
        // SAFETY: IntoBytes guarantees that Self can be cast to a byte slice.
        // And since we have exclusive ownership of self, it should be safe to
        // cast to an atomic byte slice (which can then be used by multiple
        // threads safely).
        // FromBytes guarantees that any value then assigned to these bytes
        // is still valid.
        unsafe {
            core::slice::from_raw_parts_mut(
                core::ptr::from_mut(self).cast::<AtomicU8>(),
                size_of_val(self),
            )
        }
    }
}

impl<T> AsAtomicBytes for T where T: IntoBytes + FromBytes + ?Sized + Immutable + KnownLayout {}

/// Marker trait for atomic primitives.
///
/// # Safety
///
/// Must only be implemented for types under [`core::sync::atomic`]
pub unsafe trait Atomic {}

// SAFETY: This type is under core::sync::atomic
unsafe impl Atomic for AtomicU8 {}
// SAFETY: This type is under core::sync::atomic
unsafe impl Atomic for atomic::AtomicU16 {}
// SAFETY: This type is under core::sync::atomic
unsafe impl Atomic for atomic::AtomicU32 {}
// SAFETY: This type is under core::sync::atomic
unsafe impl Atomic for atomic::AtomicU64 {}
// SAFETY: This type is under core::sync::atomic
unsafe impl Atomic for atomic::AtomicI8 {}
// SAFETY: This type is under core::sync::atomic
unsafe impl Atomic for atomic::AtomicI16 {}
// SAFETY: This type is under core::sync::atomic
unsafe impl Atomic for atomic::AtomicI32 {}
// SAFETY: This type is under core::sync::atomic
unsafe impl Atomic for atomic::AtomicI64 {}

pub trait AtomicSliceOps {
    /// # Safety
    /// The caller must ensure that `dest..dest+len` is a
    /// [valid](core::ptr#safety) target for writes.
    unsafe fn atomic_read_ptr(&self, dest: *mut u8, len: usize);

    /// # Safety
    /// The caller must ensure that `src..src+len` is a [valid](core::ptr#safety) source for reads.
    unsafe fn atomic_write_ptr(&self, src: *const u8, len: usize);

    /// Reads from the slice into `dest`.
    ///
    /// Panics if the slice is not the same size as `dest`.
    fn atomic_read(&self, dest: &mut [u8]) {
        // SAFETY: `dest` is a valid target for writes.
        unsafe { self.atomic_read_ptr(dest.as_mut_ptr(), dest.len()) }
    }

    /// Reads an object from the slice.
    ///
    /// Panics if the slice is not the same size as `T`.
    fn atomic_read_obj<T: FromBytes + Immutable + KnownLayout>(&self) -> T {
        let mut obj = mem::MaybeUninit::<T>::uninit();
        // SAFETY: `obj` is a valid target for writes, and will be initialized by
        // `atomic_read_ptr`.
        unsafe {
            self.atomic_read_ptr(obj.as_mut_ptr().cast::<u8>(), size_of::<T>());
            obj.assume_init()
        }
    }

    /// Writes `src` to the slice.
    ///
    /// Panics if the slice is not the same size as `src`.
    fn atomic_write(&self, src: &[u8]) {
        // SAFETY: `src` is a valid source for reads.
        unsafe { self.atomic_write_ptr(src.as_ptr(), src.len()) }
    }

    /// Writes an object to the slice.
    ///
    /// Panics if the slice is not the same size as `T`.
    fn atomic_write_obj<T: IntoBytes + Immutable + KnownLayout>(&self, obj: &T) {
        self.atomic_write(obj.as_bytes());
    }

    /// Fills the slice with `value`.
    fn atomic_fill(&self, value: u8);

    fn as_atomic<T: Atomic>(&self) -> Option<&T>;
    fn as_atomic_slice<T: Atomic>(&self) -> Option<&[T]>;
}

impl AtomicSliceOps for [AtomicU8] {
    unsafe fn atomic_read_ptr(&self, dest: *mut u8, len: usize) {
        assert_eq!(
            self.len(),
            len,
            "destination and source slices have different lengths"
        );
        // BUGBUG: this is undefined behavior, because
        // copy_nonoverlapping technically relies on there being no concurrent
        // mutator of `src`, and there may be here--consider whether calling
        // memcpy directly might be safer.
        unsafe { core::ptr::copy_nonoverlapping(self.as_ptr().cast::<u8>(), dest, len) }
    }

    unsafe fn atomic_write_ptr(&self, src: *const u8, len: usize) {
        assert_eq!(
            self.len(),
            len,
            "destination and source slices have different lengths"
        );
        // BUGBUG: this is undefined behavior, because
        // copy_nonoverlapping technically relies on there being no other
        // concurrent mutator of `dst`, and there may be here--consider whether
        // calling memcpy directly might be safer.
        unsafe { core::ptr::copy_nonoverlapping(src, self.as_ptr() as *mut u8, len) }
    }

    fn atomic_fill(&self, value: u8) {
        // BUGBUG: this is undefined behavior, because write_bytes
        // technically relies on there being no other concurrent accessor of
        // `dst`, and there may be here--consider whether calling memset might
        // be safer.
        unsafe { core::ptr::write_bytes(self.as_ptr() as *mut u8, value, self.len()) }
    }

    fn as_atomic<T: Atomic>(&self) -> Option<&T> {
        // SAFETY: Per https://github.com/rust-lang/unsafe-code-guidelines/issues/345
        // it *should* be fine to have mixed-size atomic accesses so long as we
        // don't do more than 16 bytes at a time. Our largest supported type is
        // 8 bytes.
        let (a, b, c) = unsafe { self.align_to() };
        if a.is_empty() && b.len() == 1 && c.is_empty() {
            Some(&b[0])
        } else {
            None
        }
    }

    fn as_atomic_slice<T: Atomic>(&self) -> Option<&[T]> {
        // SAFETY: Per https://github.com/rust-lang/unsafe-code-guidelines/issues/345
        // it *should* be fine to have mixed-size atomic accesses so long as we
        // don't do more than 16 bytes at a time. Our largest supported type is
        // 8 bytes.
        let (a, b, c) = unsafe { self.align_to() };
        if a.is_empty() && c.is_empty() {
            Some(b)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::AsAtomicBytes;
    use super::AtomicSliceOps;
    use core::sync::atomic::AtomicI8;
    use core::sync::atomic::AtomicI16;
    use core::sync::atomic::AtomicI32;
    use core::sync::atomic::AtomicI64;
    use core::sync::atomic::AtomicU8;
    use core::sync::atomic::AtomicU16;
    use core::sync::atomic::AtomicU32;
    use core::sync::atomic::AtomicU64;
    use core::sync::atomic::Ordering::Relaxed;
    use test_with_tracing::test;

    // Test: Read, overwrite, and fill an interior atomic-byte view of native
    //       integer storage.
    // Expected: Object and byte reads preserve exact values, mutations affect
    //       only the selected bytes, and surrounding guards remain intact.
    #[test]
    fn byte_and_object_operations_preserve_subslice_guards() {
        let mut storage = [
            0xfeed_f00d_dead_beef_u64,
            0x1122_3344_5566_7788,
            0x99aa_bbcc_ddee_ff00,
            0xcafe_babe_7654_3210,
        ];
        let bytes = storage.as_atomic_bytes();
        let region = &bytes[8..24];
        assert_eq!(
            region.atomic_read_obj::<[u64; 2]>(),
            [0x1122_3344_5566_7788, 0x99aa_bbcc_ddee_ff00]
        );
        region.atomic_write_obj(&[0x8877_6655_4433_2211_u64, 0x00ff_eedd_ccbb_aa99]);
        let mut read = [0u8; 16];
        region.atomic_read(&mut read);
        assert_eq!(&read[..8], &0x8877_6655_4433_2211_u64.to_ne_bytes());
        assert_eq!(&read[8..], &0x00ff_eedd_ccbb_aa99_u64.to_ne_bytes());
        region.atomic_fill(0xa5);
        assert_eq!(region.atomic_read_obj::<[u8; 16]>(), [0xa5; 16]);
        let replacement = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        region.atomic_write(&replacement);
        assert_eq!(region.atomic_read_obj::<[u8; 16]>(), replacement);
        assert_eq!(storage[0], 0xfeed_f00d_dead_beef);
        assert_eq!(storage[3], 0xcafe_babe_7654_3210);

        let mut unsized_storage = [11u8, 22, 33, 44, 55, 66];
        unsized_storage[1..5].as_atomic_bytes().atomic_fill(0x7f);
        assert_eq!(unsized_storage, [11, 0x7f, 0x7f, 0x7f, 0x7f, 66]);
    }

    // Test: Interpret aligned bytes as every supported integer atomic type.
    //       Reject incomplete or misaligned views.
    // Expected: Each atomic value matches its native byte chunk and slice
    //       lengths are exact. Invalid single-value and slice views return
    //       None.
    #[test]
    fn typed_views_preserve_bits_and_require_exact_alignment() {
        let mut storage = [0x1020_3040_5060_7080_u64, 0x91a2_b3c4_d5e6_f708];
        let native_chunks = storage.map(u64::to_ne_bytes);
        let native_bytes = native_chunks.as_flattened();
        let bytes = storage.as_atomic_bytes();
        macro_rules! check {
            ($atomic:ty, $integer:ty, $width:expr) => {
                let (native_values, _) = native_bytes.as_chunks::<$width>();
                assert_eq!(
                    bytes[..$width]
                        .as_atomic::<$atomic>()
                        .unwrap()
                        .load(Relaxed),
                    <$integer>::from_ne_bytes(native_values[0])
                );
                let atomic_slice = bytes.as_atomic_slice::<$atomic>().unwrap();
                assert_eq!(atomic_slice.len(), native_values.len());
                for (atomic, chunk) in atomic_slice.iter().zip(native_values) {
                    assert_eq!(atomic.load(Relaxed), <$integer>::from_ne_bytes(*chunk));
                }
            };
        }
        check!(AtomicU8, u8, 1);
        check!(AtomicU16, u16, 2);
        check!(AtomicU32, u32, 4);
        check!(AtomicU64, u64, 8);
        check!(AtomicI8, i8, 1);
        check!(AtomicI16, i16, 2);
        check!(AtomicI32, i32, 4);
        check!(AtomicI64, i64, 8);
        assert!(bytes.as_atomic::<AtomicU64>().is_none());
        assert!(bytes[..7].as_atomic::<AtomicU64>().is_none());
        assert!(bytes[1..9].as_atomic::<AtomicU64>().is_none());
        assert!(bytes[1..9].as_atomic_slice::<AtomicU64>().is_none());
        assert!(bytes[..15].as_atomic_slice::<AtomicU64>().is_none());
        assert!(bytes[..0].as_atomic::<AtomicU64>().is_none());
        assert!(
            bytes[..0]
                .as_atomic_slice::<AtomicU64>()
                .unwrap()
                .is_empty()
        );
    }

    // Test: Pass mismatched buffers and objects to atomic-copy APIs, then
    //       operate on an empty view.
    // Expected: Length mismatches panic before mutation. Empty operations
    //       preserve backing storage and destination guards.
    #[test]
    fn invalid_lengths_and_empty_views_preserve_storage() {
        let mut storage = [11u8, 22, 33, 44];
        let bytes = storage.as_atomic_bytes();
        for length in [3, 5] {
            let mut destination = [0xa5u8; 5];
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    bytes.atomic_read(&mut destination[..length]);
                }))
                .is_err()
            );
            assert_eq!(destination, [0xa5; 5]);
            assert!(std::panic::catch_unwind(|| bytes.atomic_write(&[0x7f; 5][..length])).is_err());
            assert_eq!(bytes.atomic_read_obj::<[u8; 4]>(), [11, 22, 33, 44]);
        }
        assert!(std::panic::catch_unwind(|| bytes.atomic_read_obj::<u64>()).is_err());
        let empty = &bytes[..0];
        empty.atomic_read(&mut []);
        empty.atomic_write(&[]);
        empty.atomic_fill(0xff);
        assert_eq!(empty.atomic_read_obj::<[u8; 0]>(), []);
        assert_eq!(bytes.atomic_read_obj::<[u8; 4]>(), [11, 22, 33, 44]);
    }
}
