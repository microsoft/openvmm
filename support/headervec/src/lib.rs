// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements the `HeaderVec` type for constructing dynamically
//! sized values that have a fixed size header and a variable sized element
//! type. This is a common pattern in IOCTL input buffers.

// UNSAFETY: Implementing a custom data structure that requires manual memory
// management and pointer manipulation.
#![expect(unsafe_code)]
#![no_std]

extern crate alloc;

use alloc::alloc::Layout;
use alloc::alloc::alloc;
use alloc::alloc::handle_alloc_error;
use alloc::boxed::Box;
use core::cmp;
use core::mem::MaybeUninit;
use core::ops::Deref;
use core::ops::DerefMut;
use core::ptr::NonNull;

/// A type that represents a fixed-sized header followed by a variable-sized
/// tail.
#[repr(C)]
#[derive(Debug)]
pub struct HeaderSlice<T, U: ?Sized> {
    /// The fixed-sized header.
    pub head: T,
    /// The variable-sized tail.
    pub tail: U,
}

impl<T, U> HeaderSlice<T, [U]> {
    fn ptr_from_raw_parts(ptr: *const T, len: usize) -> *const Self {
        // Create a [T] (the inner type doesn't actually matter) with `len`
        // elements, then cast it to a HeaderSlice<T, [U]>. The cast via `as`
        // preserves the element count.
        //
        // FUTURE: use [`core::ptr::from_raw_parts`] once it is stable.
        core::ptr::slice_from_raw_parts(ptr, len) as *const Self
    }

    fn ptr_from_raw_parts_mut(ptr: *mut T, len: usize) -> *mut Self {
        // Create a [T] (the inner type doesn't actually matter) with `len`
        // elements, then cast it to a HeaderSlice<T, [U]>. The cast via `as`
        // preserves the element count.
        //
        // FUTURE: use [`core::ptr::from_raw_parts_mut`] once it is stable.
        core::ptr::slice_from_raw_parts_mut(ptr, len) as *mut Self
    }

    /// # Safety
    /// The caller must ensure that `ptr` points to a `T` followed by `len`
    /// elements of `U`, valid for lifetime `'a`.
    unsafe fn from_raw_parts<'a>(ptr: *const T, len: usize) -> &'a Self {
        // SAFETY: the caller ensures that the resulting pointer is valid for
        // lifetime `'a`.
        unsafe { &*Self::ptr_from_raw_parts(ptr, len) }
    }

    /// # Safety
    /// The caller must ensure that `ptr` points to a `T` followed by `len`
    /// elements of `U`, valid for lifetime `'a`.
    unsafe fn from_raw_parts_mut<'a>(ptr: *mut T, len: usize) -> &'a mut Self {
        // SAFETY: the caller ensures that the resulting pointer is valid for
        // lifetime `'a`.
        unsafe { &mut *Self::ptr_from_raw_parts_mut(ptr, len) }
    }
}

#[derive(Debug)]
enum Data<T, U, const N: usize> {
    Fixed(HeaderSlice<T, [MaybeUninit<U>; N]>),
    Alloc(Box<HeaderSlice<T, [MaybeUninit<U>]>>),
}

impl<T, U, const N: usize> Data<T, U, N> {
    /// # Safety
    ///
    /// The caller must ensure that the first `len` elements have been initialized.
    unsafe fn valid(&self, len: usize) -> &HeaderSlice<T, [U]> {
        // SAFETY: the caller has ensured that the first `len` elements have been
        // initialized.
        unsafe { HeaderSlice::from_raw_parts(core::ptr::from_ref(self.storage()).cast(), len) }
    }

    /// # Safety
    ///
    /// The caller must ensure that the first `len` elements have been initialized.
    unsafe fn valid_mut(&mut self, len: usize) -> &mut HeaderSlice<T, [U]> {
        // SAFETY: the caller has ensured that the first `len` elements have been
        // initialized.
        unsafe {
            HeaderSlice::from_raw_parts_mut(core::ptr::from_mut(self.storage_mut()).cast(), len)
        }
    }

    fn storage(&self) -> &HeaderSlice<T, [MaybeUninit<U>]> {
        let p: &HeaderSlice<T, [MaybeUninit<U>]> = match self {
            Data::Fixed(p) => p,
            Data::Alloc(p) => p,
        };
        if size_of::<U>() == 0 {
            // SAFETY: the tail element is a ZST so its slice is valid for any
            // length.
            unsafe { HeaderSlice::from_raw_parts(&raw const p.head, usize::MAX) }
        } else {
            p
        }
    }

    fn storage_mut(&mut self) -> &mut HeaderSlice<T, [MaybeUninit<U>]> {
        let p: &mut HeaderSlice<T, [MaybeUninit<U>]> = match self {
            Data::Fixed(p) => p,
            Data::Alloc(p) => p,
        };
        if size_of::<U>() == 0 {
            // SAFETY: the tail element is a ZST so its slice is valid for any
            // length.
            unsafe { HeaderSlice::from_raw_parts_mut(&raw mut p.head, usize::MAX) }
        } else {
            p
        }
    }
}

/// Implements a `Vec`-like type for building structures with a fixed-sized
/// prefix before a dynamic number of elements.
///
/// To avoid allocations in common cases, the header and elements are stored
/// internally without allocating until the element count would exceed the
/// statically determined capacity.
///
/// Only a small portion of the `Vec` interface is supported. Additional methods
/// can be added as needed.
///
/// The data managed by this type must be `Copy`. This simplifies the resource
/// management and should be sufficient for most use cases.
///
/// # Example
/// ```
/// # use headervec::HeaderVec;
/// #[derive(Copy, Clone)]
/// struct Header { x: u32 }
/// let mut v = HeaderVec::<Header, u8, 10>::new(Header{ x: 1234 });
/// v.push_tail(5);
/// v.push_tail(6);
/// assert_eq!(v.head.x, 1234);
/// assert_eq!(&v.tail, &[5, 6]);
/// ```
#[derive(Debug)]
pub struct HeaderVec<T, U, const N: usize> {
    data: Data<T, U, N>,
    len: usize,
}

impl<T: Copy + Default, U: Copy, const N: usize> Default for HeaderVec<T, U, N> {
    fn default() -> Self {
        Self::new(Default::default())
    }
}

impl<T: Copy, U: Copy, const N: usize> HeaderVec<T, U, N> {
    /// Constructs a new `HeaderVec` with a header of `head` and no tail
    /// elements.
    pub fn new(head: T) -> Self {
        Self {
            data: Data::Fixed(HeaderSlice {
                head,
                tail: [const { MaybeUninit::uninit() }; N],
            }),
            len: 0,
        }
    }

    /// Constructs a new `HeaderVec` with a header of `head` and no tail
    /// elements, but with a dynamically allocated capacity for `cap` elements.
    pub fn with_capacity(head: T, cap: usize) -> Self {
        let mut vec = Self::new(head);
        if cap > vec.tail_capacity() {
            vec.realloc(cap);
        }
        vec
    }

    fn realloc(&mut self, cap: usize) {
        assert!(cap > self.len);
        assert!(size_of::<U>() > 0);

        let base_layout = Layout::new::<HeaderSlice<T, [MaybeUninit<U>; 0]>>();
        let layout = base_layout
            .extend_packed(Layout::array::<U>(cap).unwrap())
            .unwrap()
            .pad_to_align();

        // SAFETY: `layout` is correctly constructed and is non-empty.
        let alloc = unsafe { alloc(layout) };
        let Some(alloc) = NonNull::new(alloc) else {
            handle_alloc_error(layout);
        };
        // Copy the head.
        // SAFETY: `alloc` starts with `T`.
        unsafe {
            alloc.cast::<T>().write(self.data.storage_mut().head);
        }
        // Build the fat pointer to the DST.
        let alloc =
            HeaderSlice::<T, [MaybeUninit<U>]>::ptr_from_raw_parts_mut(alloc.as_ptr().cast(), cap);
        // SAFETY: `head` has been initialized and `tail` is `MaybeUninit`.
        // `alloc` was allocated with the same layout `Box::new` would use.
        let mut alloc = unsafe { Box::from_raw(alloc) };
        // Copy the initialized portion of the tail.
        alloc.tail[..self.len].copy_from_slice(&self.data.storage_mut().tail[..self.len]);
        self.data = Data::Alloc(alloc);
    }

    fn extend_tail(&mut self, n: usize) -> &mut [MaybeUninit<U>] {
        let cap = self.tail_capacity();
        if cap - self.len < n {
            assert!(size_of::<U>() > 0, "ZST tail slice overflow");
            // Double the current capacity to ensure a geometric progression
            // (avoiding O(n^2) allocations).
            let new_cap = cmp::max(cmp::max(8, cap.strict_mul(2)), self.len.strict_add(n));
            self.realloc(new_cap);
        }
        &mut self.spare_tail_capacity_mut()[..n]
    }

    /// Reserves capacity for at least `n` additional tail elements.
    pub fn reserve_tail(&mut self, n: usize) {
        self.extend_tail(n);
    }

    /// Returns the remaining spare capacity of the tail as a slice of
    /// `MaybeUninit<U>`.
    ///
    /// The returned slice can be used to fill the tail with data before marking
    /// the data as initialized using [`Self::set_tail_len`].
    pub fn spare_tail_capacity_mut(&mut self) -> &mut [MaybeUninit<U>] {
        &mut self.data.storage_mut().tail[self.len..]
    }

    /// Pushes a tail element, reallocating if necessary.
    pub fn push_tail(&mut self, val: U) {
        // For zero-sized types (unlikely to be useful but hard to prohibit),
        // just increment len.
        if size_of_val(&val) > 0 {
            self.extend_tail(1)[0].write(val);
        }
        self.len += 1;
    }

    /// Extends the tail elements from the given slice.
    pub fn extend_tail_from_slice(&mut self, other: &[U]) {
        self.extend_tail(other.len()).write_copy_of_slice(other);
        self.len += other.len();
    }

    /// Retrieves a pointer to the head. The tail is guaranteed to immediately
    /// after the head (with appropriate padding).
    pub fn as_ptr(&self) -> *const T {
        core::ptr::from_ref(self.data.storage()).cast()
    }

    /// Retrieves a mutable pointer to the head. The tail is guaranteed to
    /// immediately after the head (with appropriate padding).
    pub fn as_mut_ptr(&mut self) -> *mut T {
        core::ptr::from_mut(self.data.storage_mut()).cast()
    }

    /// Returns the number of tail elements that can be stored without
    /// reallocating.
    pub fn tail_capacity(&self) -> usize {
        self.data.storage().tail.len()
    }

    /// Sets the number of tail elements to 0.
    pub fn clear_tail(&mut self) {
        self.len = 0;
    }

    /// Truncates the tail to `len` elements. Has no effect if there are already
    /// fewer than `len` tail elements.
    pub fn truncate_tail(&mut self, len: usize) {
        if len < self.len {
            self.len = len;
        }
    }

    /// Sets the number of tail elements.
    ///
    /// Panics if `len` is greater than the capacity.
    ///
    /// # Safety
    ///
    /// The caller must ensure that all `len` elements have been initialized.
    pub unsafe fn set_tail_len(&mut self, len: usize) {
        assert!(len <= self.tail_capacity());
        self.len = len;
    }

    /// Returns the total contiguous byte length of the structure, including
    /// both the head and tail elements.
    pub fn total_byte_len(&self) -> usize {
        size_of_val(&**self)
    }

    /// Returns the total contiguous byte length of the structure, including
    /// both the head and tail elements, including the tail's capacity.
    pub fn total_byte_capacity(&self) -> usize {
        size_of_val(self.data.storage())
    }
}

impl<T, U, const N: usize> Deref for HeaderVec<T, U, N> {
    type Target = HeaderSlice<T, [U]>;
    fn deref(&self) -> &Self::Target {
        // SAFETY: `self.len` tail elements have been initialized.
        unsafe { self.data.valid(self.len) }
    }
}

impl<T, U, const N: usize> DerefMut for HeaderVec<T, U, N> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: `self.len` tail elements have been initialized.
        unsafe { self.data.valid_mut(self.len) }
    }
}

impl<T: Copy, U: Copy, const N: usize> Extend<U> for HeaderVec<T, U, N> {
    fn extend<I: IntoIterator<Item = U>>(&mut self, iter: I) {
        for item in iter {
            self.push_tail(item);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::HeaderSlice;
    use super::HeaderVec;
    use alloc::vec::Vec;
    use core::fmt::Debug;
    use test_with_tracing::test;

    fn test<T: Copy + Eq + Debug, U: Copy + Eq + Debug, const N: usize>(
        head: T,
        vals: impl IntoIterator<Item = U>,
    ) {
        let vals = Vec::from_iter(vals);
        // Push
        {
            let mut v: HeaderVec<T, U, N> = HeaderVec::new(head);
            for &i in &vals {
                v.push_tail(i);
            }
            assert_eq!(v.head, head);
            assert_eq!(&v.tail, vals.as_slice());
        }
        // Extend from slice
        {
            let mut v: HeaderVec<T, U, N> = HeaderVec::new(head);
            v.extend_tail_from_slice(&[]);
            assert!(v.tail.is_empty());
            v.extend_tail_from_slice(&vals);
            v.extend_tail_from_slice(&[]);
            assert_eq!(v.head, head);
            assert_eq!(&v.tail, vals.as_slice());
        }
        // Extend
        {
            let mut v: HeaderVec<T, U, N> = HeaderVec::new(head);
            v.extend(vals.iter().copied());
            assert_eq!(v.head, head);
            assert_eq!(&v.tail, vals.as_slice());
        }
        // Reserve + set_len
        {
            let mut v: HeaderVec<T, U, N> = HeaderVec::new(head);
            v.reserve_tail(vals.len());
            if size_of::<U>() > 0 {
                assert_eq!(
                    v.tail_capacity(),
                    if size_of::<U>() == 0 {
                        usize::MAX
                    } else {
                        vals.len()
                    }
                );
            }
            for (s, d) in vals.iter().copied().zip(v.spare_tail_capacity_mut()) {
                d.write(s);
            }
            // SAFETY: all elements are initialized.
            unsafe { v.set_tail_len(vals.len()) };
            assert_eq!(v.head, head);
            assert_eq!(&v.tail, vals.as_slice());
        }
    }

    #[test]
    fn test_push() {
        test::<u8, u32, 3>(0x10, 0..200);
    }

    #[test]
    fn test_zero_array() {
        test::<u8, u32, 0>(0x10, 0..200);
    }

    #[test]
    fn test_with_capacity_and_growth() {
        for requested in [0, 3, 5] {
            let mut v = HeaderVec::<u8, u32, 3>::with_capacity(0x10, requested);
            assert!(v.tail.is_empty());
            assert_eq!(v.head, 0x10);
            let capacity = v.tail_capacity();
            assert!(capacity >= requested);
            let ptr = v.as_ptr();
            let vals: Vec<_> = (0..capacity as u32).collect();
            v.extend_tail_from_slice(&vals);
            assert_eq!(v.as_ptr(), ptr);
            assert_eq!(&v.tail, vals.as_slice());

            v.head = 0x20;
            v.tail[0] = 100;
            v.push_tail(200);
            assert!(v.tail_capacity() > capacity);
            assert_eq!(v.head, 0x20);
            assert_eq!(v.tail[0], 100);
            assert_eq!(&v.tail[1..capacity], &vals[1..]);
            assert_eq!(v.tail[capacity], 200);
            assert_eq!(v.tail.len(), capacity + 1);
        }
    }

    #[test]
    fn test_truncate_and_clear_reuse_capacity() {
        for capacity in [3, 5] {
            let mut v = HeaderVec::<u8, u32, 3>::with_capacity(0x10, capacity);
            v.extend_tail_from_slice(&[1, 2, 3]);
            let ptr = v.as_ptr();

            v.truncate_tail(3);
            v.truncate_tail(usize::MAX);
            assert_eq!(&v.tail, &[1, 2, 3]);
            v.truncate_tail(1);
            assert_eq!(&v.tail, &[1]);
            v.extend_tail_from_slice(&[4, 5]);
            assert_eq!(&v.tail, &[1, 4, 5]);

            v.clear_tail();
            assert!(v.tail.is_empty());
            v.push_tail(9);
            assert_eq!(&v.tail, &[9]);
            assert_eq!(v.head, 0x10);
            assert_eq!(v.tail_capacity(), capacity);
            assert_eq!(v.as_ptr(), ptr);
        }
    }

    #[test]
    fn test_contiguous_layout() {
        for capacity in [3, 5] {
            let mut v = HeaderVec::<u8, u32, 3>::with_capacity(0x10, capacity);
            assert_eq!(v.total_byte_len(), 4);
            assert_eq!(v.total_byte_capacity(), 4 + capacity * 4);
            v.extend_tail_from_slice(&[1, 2]);
            assert_eq!(v.total_byte_len(), size_of::<HeaderSlice<u8, [u32; 2]>>());
            // SAFETY: the buffer has a u8 header and two initialized u32 elements.
            let buffer = unsafe { &*v.as_ptr().cast::<HeaderSlice<u8, [u32; 2]>>() };
            assert_eq!(buffer.head, 0x10);
            assert_eq!(buffer.tail, [1, 2]);
            // SAFETY: the same layout is valid for exclusive access through v.
            let buffer = unsafe { &mut *v.as_mut_ptr().cast::<HeaderSlice<u8, [u32; 2]>>() };
            buffer.head = 0x20;
            buffer.tail[1] = 3;
            assert_eq!(v.head, 0x20);
            assert_eq!(&v.tail, &[1, 3]);
            assert_eq!(v.total_byte_capacity(), 4 + capacity * 4);
        }

        for capacity in [9, 17] {
            let mut v = HeaderVec::<u64, u8, 9>::with_capacity(0x10, capacity);
            assert_eq!(v.total_byte_len(), 8);
            v.extend_tail_from_slice(&[1, 2, 3]);
            assert_eq!(v.total_byte_len(), 16);
            assert_eq!(v.total_byte_capacity(), 8 + capacity.next_multiple_of(8));
            v.truncate_tail(0);
            assert_eq!(v.total_byte_len(), 8);
            assert_eq!(v.total_byte_capacity(), 8 + capacity.next_multiple_of(8));
        }
    }

    #[test]
    fn test_overaligned_layout_and_growth() {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[repr(align(64))]
        struct Aligned(u8);

        fn check<T: Copy + Debug + Eq, U: Copy + Debug + Eq>(head: T, tail: U) {
            let mut v = HeaderVec::<T, U, 0>::with_capacity(head, 3);
            assert_eq!(v.tail_capacity(), 3);
            assert_eq!(v.total_byte_capacity(), size_of::<HeaderSlice<T, [U; 3]>>());
            assert!(
                v.as_ptr()
                    .addr()
                    .is_multiple_of(align_of::<T>().max(align_of::<U>()))
            );
            v.extend_tail_from_slice(&[tail; 3]);
            assert_eq!(v.total_byte_len(), size_of::<HeaderSlice<T, [U; 3]>>());
            assert!(v.tail.as_ptr().addr().is_multiple_of(align_of::<U>()));

            v.push_tail(tail);
            assert_eq!(v.tail_capacity(), 8);
            assert_eq!(v.total_byte_capacity(), size_of::<HeaderSlice<T, [U; 8]>>());
            assert_eq!(v.total_byte_len(), size_of::<HeaderSlice<T, [U; 4]>>());
            assert!(
                v.as_ptr()
                    .addr()
                    .is_multiple_of(align_of::<T>().max(align_of::<U>()))
            );
            assert!(v.tail.as_ptr().addr().is_multiple_of(align_of::<U>()));
            assert_eq!(v.head, head);
            assert_eq!(v.tail, [tail; 4]);
        }

        check(Aligned(1), 2u8);
        check(1u8, Aligned(2));
    }

    #[test]
    #[should_panic]
    fn test_capacity_overflow() {
        let _ = HeaderVec::<u8, u64, 0>::with_capacity(0, usize::MAX);
    }

    #[test]
    #[should_panic]
    fn test_capacity_with_header_overflow() {
        let _ = HeaderVec::<u64, u8, 0>::with_capacity(0, isize::MAX as usize);
    }

    #[test]
    fn test_zst_tail_reuse() {
        let mut v = HeaderVec::<u32, (), 0>::with_capacity(0x10, usize::MAX);
        v.extend_tail_from_slice(&[(); 3]);
        v.head = 0x20;
        v.truncate_tail(1);
        assert_eq!(v.tail.len(), 1);
        v.extend_tail_from_slice(&[(); 2]);
        assert_eq!(v.tail.len(), 3);
        v.clear_tail();
        assert!(v.tail.is_empty());
        v.push_tail(());
        assert_eq!(v.tail.len(), 1);
        assert_eq!(v.head, 0x20);
        assert_eq!(v.tail_capacity(), usize::MAX);
        assert_eq!(v.total_byte_len(), 4);
        assert_eq!(v.total_byte_capacity(), 4);
    }

    #[test]
    fn test_zst_head() {
        test::<(), u32, 3>((), 0..200);
    }

    #[test]
    fn test_zst_tail() {
        test::<u8, (), 0>(0x10, (0..200).map(|_| ()));
    }

    #[test]
    fn test_zst_both() {
        test::<(), (), 0>((), (0..200).map(|_| ()));
    }

    #[test]
    #[should_panic(expected = "ZST tail slice overflow")]
    fn test_zst_overflow() {
        let mut v: HeaderVec<u8, (), 0> = HeaderVec::new(0);
        v.push_tail(());
        v.extend_tail_from_slice(&[(); usize::MAX]);
    }
}
