// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements the `HeaderVec` type for constructing dynamically
//! sized values that have a fixed size header and a variable sized element
//! type. This is a common pattern in IOCTL input buffers.

// UNSAFETY: Implementing a custom data structure that requires manual memory
// management and pointer manipulation.
#![allow(unsafe_code)]
#![allow(clippy::undocumented_unsafe_blocks)]

use std::alloc::Layout;
use std::alloc::{self};
use std::cmp;
use std::mem::MaybeUninit;
use std::ops::Deref;
use std::ops::DerefMut;
use std::ops::Index;
use std::ops::IndexMut;
use std::ptr::NonNull;
use std::slice::Iter;
use std::slice::IterMut;
use std::slice::SliceIndex;

/// Trait implemented by fixed-sized arrays that can be used as the element type
/// for HeaderVec. Once Rust supports const generics, this can be removed.
///
/// # Safety
///
/// Must only be implemented on fixed size arrays (i.e: `[T; N]`)
pub unsafe trait FixedArray {
    type Element: Copy;
    const COUNT: usize;
}

// SAFETY: Only implementing for fixed size arrays.
unsafe impl<T: Copy, const N: usize> FixedArray for [T; N] {
    type Element = T;
    const COUNT: usize = N;
}

#[repr(C)]
#[derive(Debug)]
struct Combined<T, U> {
    head: T,
    tail: MaybeUninit<U>,
}

#[derive(Debug)]
enum Data<T, U: FixedArray> {
    Fixed(Combined<T, U>),
    Alloc(NonNull<Combined<T, U>>, usize),
}

// SAFETY: Data essentially has non-thread-specific ownership of (T, [U]), so it
// is Send + Sync if T and U are Send + Sync.
unsafe impl<T, U: FixedArray> Send for Data<T, U>
where
    T: Send,
    U: Send,
{
}
// SAFETY: See above comment
unsafe impl<T, U: FixedArray> Sync for Data<T, U>
where
    T: Sync,
    U: Sync,
{
}

impl<T, U: FixedArray> Data<T, U> {
    fn head(&self) -> &T {
        match self {
            Data::Fixed(Combined { head, .. }) => head,
            Data::Alloc(p, _) => unsafe { &p.as_ref().head },
        }
    }

    fn head_mut(&mut self) -> &mut T {
        match self {
            Data::Fixed(Combined { head, .. }) => head,
            Data::Alloc(p, _) => unsafe { &mut p.as_mut().head },
        }
    }

    /// SAFETY: the caller must ensure that the first `len` elements have been
    /// initialized.
    unsafe fn tail(&self, len: usize) -> &[U::Element] {
        match self {
            Data::Fixed(Combined { tail, .. }) => {
                assert!(len <= U::COUNT || size_of::<U::Element>() == 0);
                unsafe { std::slice::from_raw_parts(tail.as_ptr().cast::<U::Element>(), len) }
            }
            Data::Alloc(p, cap) => {
                assert!(len <= *cap);
                unsafe {
                    std::slice::from_raw_parts(p.as_ref().tail.as_ptr().cast::<U::Element>(), len)
                }
            }
        }
    }

    /// SAFETY: the caller must ensure that the first `len` elements have been
    /// initialized.
    unsafe fn tail_mut(&mut self, len: usize) -> &mut [U::Element] {
        match self {
            Data::Fixed(Combined { tail, .. }) => {
                assert!(len <= U::COUNT || size_of::<U::Element>() == 0);
                unsafe { std::slice::from_raw_parts_mut(tail.as_ptr() as *mut U::Element, len) }
            }
            Data::Alloc(p, cap) => {
                assert!(len <= *cap);
                unsafe {
                    std::slice::from_raw_parts_mut(p.as_ref().tail.as_ptr() as *mut U::Element, len)
                }
            }
        }
    }

    fn capacity(&self) -> usize {
        match self {
            Data::Fixed(_) => U::COUNT,
            Data::Alloc(_, cap) => *cap,
        }
    }

    fn tail_mut_uninit(&mut self) -> &mut [MaybeUninit<U::Element>] {
        match self {
            Data::Fixed(Combined { tail, .. }) => unsafe {
                std::slice::from_raw_parts_mut(
                    tail.as_mut_ptr().cast::<MaybeUninit<U::Element>>(),
                    U::COUNT,
                )
            },
            Data::Alloc(p, cap) => unsafe {
                std::slice::from_raw_parts_mut(
                    p.as_mut()
                        .tail
                        .as_mut_ptr()
                        .cast::<MaybeUninit<U::Element>>(),
                    *cap,
                )
            },
        }
    }

    /// Compute the allocation layout for `cap` elements.
    fn layout(cap: usize) -> Layout {
        assert!(size_of::<U::Element>() > 0);
        assert!(cap > U::COUNT);

        let base_layout = Layout::new::<Combined<T, [U::Element; 0]>>();
        Layout::from_size_align(
            base_layout
                .size()
                .checked_add(size_of::<U::Element>().checked_mul(cap).unwrap())
                .unwrap(),
            base_layout.align(),
        )
        .unwrap()
    }

    /// Returns a pointer to the start of the [Combined] data that may
    /// either be inline or dynamically allocated.
    fn data_start_ptr(&self) -> *const Combined<T, U> {
        match self {
            Data::Fixed(combined_ref) => combined_ref,
            Data::Alloc(p, _) => p.as_ptr(),
        }
    }
}

impl<T, U: FixedArray> Drop for Data<T, U> {
    fn drop(&mut self) {
        match self {
            Data::Fixed(_) => (),
            Data::Alloc(p, cap) => unsafe {
                alloc::dealloc(p.as_ptr().cast::<u8>(), Self::layout(*cap))
            },
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
/// # use pal::HeaderVec;
/// #[derive(Copy, Clone)]
/// struct Header { x: u32 }
/// let mut v = HeaderVec::<Header, [u8; 10]>::new(Header{ x: 1234 });
/// v.push(5);
/// v.push(6);
/// assert_eq!(v.x, 1234);
/// assert_eq!(&v[..], &[5, 6]);
/// ```
#[derive(Debug)]
pub struct HeaderVec<T, U: FixedArray> {
    data: Data<T, U>,
    len: usize,
}

impl<T: Copy + Default, U: FixedArray> Default for HeaderVec<T, U> {
    fn default() -> Self {
        Self::new(Default::default())
    }
}

impl<T: Copy, U: FixedArray> HeaderVec<T, U> {
    /// Constructs a new `HeaderVec` with a header of `head` and no tail
    /// elements.
    pub fn new(head: T) -> Self {
        Self {
            data: Data::Fixed(Combined {
                head,
                tail: MaybeUninit::uninit(),
            }),
            len: 0,
        }
    }

    /// Constructs a new `HeaderVec` with a header of `head` and no tail
    /// elements, but with a dynamically allocated capacity for `cap` elements.
    pub fn with_capacity(head: T, cap: usize) -> Self {
        let mut vec = Self::new(head);
        if cap > U::COUNT {
            vec.realloc(cap);
        }
        vec
    }

    fn realloc(&mut self, cap: usize) {
        assert!(cap > self.len);

        let layout = Data::<T, U>::layout(cap);
        unsafe {
            let alloc = alloc::alloc(layout).cast::<Combined<T, U>>();
            if let Some(alloc) = NonNull::new(alloc) {
                // Copy the old header and elements.
                alloc.as_ptr().cast::<u8>().copy_from(
                    self.data.data_start_ptr().cast::<u8>(),
                    self.total_byte_len(),
                );

                self.data = Data::Alloc(alloc, cap);
            } else {
                alloc::handle_alloc_error(layout);
            }
        }
    }

    fn extend_tail(&mut self, n: usize) -> &mut [MaybeUninit<U::Element>] {
        let cap = self.capacity();
        if cap - self.len < n {
            // Double the current capacity to ensure a geometric progression
            // (avoiding O(n^2) allocations).
            let new_cap = cmp::max(
                cmp::max(8, cap.checked_mul(2).unwrap()),
                self.len.checked_add(n).unwrap(),
            );
            self.realloc(new_cap);
        }
        &mut self.data.tail_mut_uninit()[self.len..self.len + n]
    }

    pub fn reserve(&mut self, n: usize) {
        self.extend_tail(n);
    }

    /// Returns the remaining spare capacity of the tail as a slice of
    /// `MaybeUninit<U::Element>`.
    ///
    /// The returned slice can be used to fill the tail with data before marking
    /// the data as initialized using [`Self::set_len].
    pub fn spare_capacity_mut(&mut self) -> &mut [MaybeUninit<U::Element>] {
        &mut self.data.tail_mut_uninit()[self.len..]
    }

    /// Pushes a tail element, reallocating if necessary.
    pub fn push(&mut self, val: U::Element) {
        // For zero-sized types (unlikely to be useful but hard to prohibit),
        // just increment len.
        if size_of_val(&val) > 0 {
            unsafe {
                self.extend_tail(1)[0].as_mut_ptr().write(val);
            }
        }
        self.len += 1;
    }

    /// Extends the tail elements from the given slice.
    pub fn extend_from_slice(&mut self, other: &[U::Element]) {
        if size_of::<U::Element>() > 0 && !other.is_empty() {
            unsafe {
                std::ptr::copy(
                    other.as_ptr(),
                    self.extend_tail(other.len())[0].as_mut_ptr(),
                    other.len(),
                );
            }
        }
        self.len += other.len();
    }

    /// Retrieves a pointer to the head. The tail is guaranteed to immediately
    /// after the head (with appropriate padding).
    pub fn as_ptr(&self) -> *const T {
        self.data.head()
    }

    /// Retrieves a mutable pointer to the head. The tail is guaranteed to
    /// immediately after the head (with appropriate padding).
    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.data.head_mut()
    }

    /// Returns a slice of the tail elements.
    pub fn as_slice(&self) -> &[U::Element] {
        unsafe { self.data.tail(self.len) }
    }

    /// Returns a mutable slice of the tail elements.
    pub fn as_mut_slice(&mut self) -> &mut [U::Element] {
        unsafe { self.data.tail_mut(self.len) }
    }

    /// Returns the number of tail elements.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn capacity(&self) -> usize {
        self.data.capacity()
    }

    /// Returns `true` if there are no tail elements.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Sets the number of tail elements to 0.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Truncates the tail to `len` elements. Has no effect if there are already
    /// fewer than `len` tail elements.
    pub fn truncate(&mut self, len: usize) {
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
    pub unsafe fn set_len(&mut self, len: usize) {
        assert!(len <= self.capacity());
        self.len = len;
    }

    /// Returns the total contiguous byte length of the structure, including
    /// both the head and tail elements.
    pub fn total_byte_len(&self) -> usize {
        // N.B. this calculation cannot overflow unless len is corrupted.
        size_of::<Combined<T, [U::Element; 0]>>() + size_of::<U::Element>() * self.len
    }

    /// Returns the total contiguous byte length of the structure, including
    /// both the head and tail elements, including the tail's capacity.
    pub fn total_byte_capacity(&self) -> usize {
        // N.B. this calculation cannot overflow unless len is corrupted.
        size_of::<Combined<T, [U::Element; 0]>>() + size_of::<U::Element>() * self.capacity()
    }

    /// Returns an iterator of the tail elements.
    pub fn iter(&self) -> Iter<'_, U::Element> {
        self.as_slice().iter()
    }

    /// Returns a mutable iterator of the tail elements.
    pub fn iter_mut(&mut self) -> IterMut<'_, U::Element> {
        self.as_mut_slice().iter_mut()
    }
}

impl<T: Copy, U: FixedArray, I: SliceIndex<[U::Element]>> Index<I> for HeaderVec<T, U> {
    type Output = I::Output;
    fn index(&self, index: I) -> &Self::Output {
        self.as_slice().index(index)
    }
}

impl<T: Copy, U: FixedArray, I: SliceIndex<[U::Element]>> IndexMut<I> for HeaderVec<T, U> {
    fn index_mut(&mut self, index: I) -> &mut Self::Output {
        self.as_mut_slice().index_mut(index)
    }
}

impl<T, U: FixedArray> Deref for HeaderVec<T, U> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        self.data.head()
    }
}

impl<T, U: FixedArray> DerefMut for HeaderVec<T, U> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.data.head_mut()
    }
}

impl<T: Copy, U: FixedArray> Extend<U::Element> for HeaderVec<T, U> {
    fn extend<I: IntoIterator<Item = U::Element>>(&mut self, iter: I) {
        for item in iter {
            self.push(item);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FixedArray;
    use super::HeaderVec;
    use std::fmt::Debug;

    fn test<T: Copy + Eq + Debug, U: FixedArray>(head: T, vals: Vec<U::Element>)
    where
        U::Element: Eq + Debug,
    {
        let mut v: HeaderVec<T, U> = HeaderVec::new(head);
        for i in vals.iter() {
            v.push(*i);
        }
        assert_eq!(*v, head);
        assert_eq!(v.as_slice(), vals.as_slice());
    }

    #[test]
    fn test_push() {
        test::<u8, [u32; 3]>(0x10, (0..200).collect());
    }

    #[test]
    fn test_zero_array() {
        test::<u8, [u32; 0]>(0x10, (0..200).collect());
    }

    #[test]
    fn test_zst_head() {
        test::<(), [u32; 3]>((), (0..200).collect());
    }

    #[test]
    fn test_zst_tail() {
        test::<u8, [(); 0]>(0x10, (0..200).map(|_| ()).collect());
    }

    #[test]
    fn test_zst_both() {
        test::<(), [(); 0]>((), (0..200).map(|_| ()).collect());
    }
}
