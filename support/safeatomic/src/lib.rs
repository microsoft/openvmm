// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![no_std]
// UNSAFETY: Manual pointer manipulation and transmutes to/from atomic types.
#![expect(unsafe_code)]
#![allow(clippy::undocumented_unsafe_blocks)]

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
