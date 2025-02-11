// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements the `HeaderVec` type for constructing dynamically
//! sized values that have a fixed size header and a variable sized element
//! type. This is a common pattern in IOCTL input buffers.

// UNSAFETY: Implementing a custom data structure that requires manual memory
// management and pointer manipulation.
#![expect(unsafe_code)]
#![warn(missing_docs)]
#![no_std]

extern crate alloc;

use alloc::alloc::alloc;
use alloc::alloc::handle_alloc_error;
use alloc::alloc::Layout;
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

#[derive(Debug)]
enum Data<T, U, const N: usize> {
    Fixed(HeaderSlice<T, [MaybeUninit<U>; N]>),
    Alloc(Box<HeaderSlice<T, [MaybeUninit<U>]>>),
}

impl<T, U, const N: usize> Data<T, U, N> {
    /// SAFETY: the caller must ensure that the first `len` elements have been
    /// initialized.
    unsafe fn valid(&self, len: usize) -> &HeaderSlice<T, [U]> {
        // Create a [U], then cast it to a HeaderSlice<T, [U]>. The cast via
        // `as` preserves the element count.
        let data =
            core::ptr::slice_from_raw_parts(core::ptr::from_ref(self.storage()).cast::<U>(), len)
                as *const HeaderSlice<T, [U]>;
        // SAFETY: the caller has ensured that the first `len` elements have been
        // initialized.
        unsafe { &*data }
    }

    /// SAFETY: the caller must ensure that the first `len` elements have been
    /// initialized.
    unsafe fn valid_mut(&mut self, len: usize) -> &mut HeaderSlice<T, [U]> {
        // Create a [U], then cast it to a HeaderSlice<T, [U]>. The cast via
        // `as` preserves the element count.
        let data = core::ptr::slice_from_raw_parts_mut(
            core::ptr::from_mut(self.storage_mut()).cast::<U>(),
            len,
        ) as *mut HeaderSlice<T, [U]>;
        // SAFETY: the caller has ensured that the first `len` elements have been
        // initialized.
        unsafe { &mut *data }
    }

    fn storage(&self) -> &HeaderSlice<T, [MaybeUninit<U>]> {
        match self {
            Data::Fixed(p) => p,
            Data::Alloc(p) => p,
        }
    }

    fn storage_mut(&mut self) -> &mut HeaderSlice<T, [MaybeUninit<U>]> {
        match self {
            Data::Fixed(p) => p,
            Data::Alloc(p) => p,
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
/// v.push(5);
/// v.push(6);
/// assert_eq!(v.x, 1234);
/// assert_eq!(&v[..], &[5, 6]);
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
        if cap > N {
            vec.realloc(cap);
        }
        vec
    }

    fn realloc(&mut self, cap: usize) {
        assert!(cap > self.len);
        assert!(size_of::<U>() > 0);

        let base_layout = Layout::new::<HeaderSlice<T, [MaybeUninit<U>; 0]>>();
        let layout = Layout::from_size_align(
            base_layout
                .size()
                .checked_add(size_of::<U>().checked_mul(cap).unwrap())
                .unwrap(),
            base_layout.align(),
        )
        .unwrap();

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
        // Convert the slice to the DST.
        let alloc =
            core::ptr::slice_from_raw_parts_mut(alloc.cast::<MaybeUninit<U>>().as_ptr(), cap)
                as *mut HeaderSlice<T, [MaybeUninit<U>]>;
        // SAFETY: `head` has been initialized and `tail` is `MaybeUninit`.
        // `alloc` was allocated with the same layout `Box::new` would use.
        let mut alloc = unsafe { Box::from_raw(alloc) };
        // Copy the tail.
        alloc.tail[..self.len].copy_from_slice(&self.data.storage_mut().tail[..self.len]);
        self.data = Data::Alloc(alloc);
    }

    fn extend_tail(&mut self, n: usize) -> &mut [MaybeUninit<U>] {
        let cap = self.tail_capacity();
        if cap - self.len < n {
            // Double the current capacity to ensure a geometric progression
            // (avoiding O(n^2) allocations).
            let new_cap = cmp::max(
                cmp::max(8, cap.checked_mul(2).unwrap()),
                self.len.checked_add(n).unwrap(),
            );
            self.realloc(new_cap);
        }
        &mut self.data.storage_mut().tail[self.len..self.len + n]
    }

    /// Reserves capacity for at least `n` additional tail elements.
    pub fn reserve(&mut self, n: usize) {
        self.extend_tail(n);
    }

    /// Returns the remaining spare capacity of the tail as a slice of
    /// `MaybeUninit<U>`.
    ///
    /// The returned slice can be used to fill the tail with data before marking
    /// the data as initialized using [`Self::set_len].
    pub fn spare_capacity_mut(&mut self) -> &mut [MaybeUninit<U>] {
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
        if size_of::<U>() > 0 && !other.is_empty() {
            // SAFETY: `[MaybeUninit<U>]` and `[U]` have the same layout.
            let other = unsafe { core::mem::transmute::<&[U], &[MaybeUninit<U>]>(other) };
            self.extend_tail(other.len()).copy_from_slice(other);
        }
        self.len += other.len();
    }

    /// Retrieves a pointer to the head. The tail is guaranteed to immediately
    /// after the head (with appropriate padding).
    pub fn as_ptr(&self) -> *const T {
        &self.head
    }

    /// Retrieves a mutable pointer to the head. The tail is guaranteed to
    /// immediately after the head (with appropriate padding).
    pub fn as_mut_ptr(&mut self) -> *mut T {
        &mut self.head
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
    use super::HeaderVec;
    use core::fmt::Debug;

    fn test<T: Copy + Eq + Debug, U: Copy, const N: usize>(head: T, vals: Vec<U>)
    where
        U: Eq + Debug,
    {
        let mut v: HeaderVec<T, U, N> = HeaderVec::new(head);
        for i in vals.iter() {
            v.push_tail(*i);
        }
        assert_eq!(v.head, head);
        assert_eq!(&v.tail, vals.as_slice());
    }

    #[test]
    fn test_push() {
        test::<u8, u32, 3>(0x10, (0..200).collect());
    }

    #[test]
    fn test_zero_array() {
        test::<u8, u32, 0>(0x10, (0..200).collect());
    }

    #[test]
    fn test_zst_head() {
        test::<(), u32, 3>((), (0..200).collect());
    }

    #[test]
    fn test_zst_tail() {
        test::<u8, (), 0>(0x10, (0..200).map(|_| ()).collect());
    }

    #[test]
    fn test_zst_both() {
        test::<(), (), 0>((), (0..200).map(|_| ()).collect());
    }
}
