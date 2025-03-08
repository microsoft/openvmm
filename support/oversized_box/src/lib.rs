// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This crate provides a `Box`-like type that is allocated larger than
//! necessary.
//!
//! This allows it to be reused for objects of different sizes without
//! reallocating.

// UNSAFETY: Manual memory management and pointer manipulation.
#![expect(unsafe_code)]
#![warn(missing_docs)]

use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ops::Deref;
use std::ops::DerefMut;
use std::pin::Pin;
use std::ptr::NonNull;

/// A `Box` for `T` with an allocation whose size and alignment is the same as
/// `S`.
pub struct OversizedBox<T: ?Sized, S> {
    ptr: NonNull<T>,
    phantom: PhantomData<*mut S>,
}

struct AssertFits<T, S>(PhantomData<(T, S)>);

impl<T, S> AssertFits<T, S> {
    const ASSERT: bool = {
        if size_of::<T>() > size_of::<S>() {
            panic!("T does not fit in S");
        }
        if align_of::<T>() > align_of::<S>() {
            panic!("T has greater alignment than S does");
        }
        true
    };
}

// SAFETY: passing through Send from T.
unsafe impl<T: ?Sized + Send, S> Send for OversizedBox<T, S> {}
// SAFETY: passing through Sync from T.
unsafe impl<T: ?Sized + Sync, S> Sync for OversizedBox<T, S> {}
impl<T: ?Sized, S> Unpin for OversizedBox<T, S> {}

impl<T, S> OversizedBox<T, S> {
    /// Allocates a new box and inserts `t`.
    ///
    /// ```rust
    /// # use oversized_box::OversizedBox;
    /// OversizedBox::<_, u64>::new(0u32);
    /// ```
    ///
    /// Fails to compile if `T`'s size or alignment is larger than `S`'s.
    ///
    /// ```compile_fail
    /// # use oversized_box::OversizedBox;
    /// OversizedBox::<_, u32>::new(0u64);
    /// ```
    pub fn new(t: T) -> Self {
        let _ = AssertFits::<T, S>::ASSERT;
        let ptr = Box::into_raw(Box::new(MaybeUninit::<S>::uninit())).cast::<T>();
        // SAFETY: `ptr` is a valid write target for `t`.
        unsafe { ptr.write(t) };
        Self {
            ptr: NonNull::new(ptr).unwrap(),
            phantom: PhantomData,
        }
    }

    /// Allocates a new box, inserts `t`, and pins the box.
    pub fn pin(t: T) -> Pin<Self> {
        Self::into_pin(Self::new(t))
    }
}

impl<T: ?Sized, S> OversizedBox<T, S> {
    /// Drops the current contents of the box, then replaces them with `t`.
    ///
    /// Returns the new box, which may have a different type from the current
    /// one.
    ///
    /// Panics if `T2`'s size or alignment is larger than `S`'s.
    pub fn refill<T2>(this: Self, t: T2) -> OversizedBox<T2, S> {
        let _ = AssertFits::<T2, S>::ASSERT;
        // SAFETY: `ptr` uniquely owns the T we are dropping.
        unsafe { std::ptr::drop_in_place(this.ptr.as_ptr()) };

        let other = OversizedBox {
            ptr: this.ptr.cast::<T2>(),
            phantom: PhantomData,
        };
        std::mem::forget(this);

        // SAFETY: `ptr` is now a valid target for writes of T2.
        unsafe { other.ptr.as_ptr().write(t) };
        other
    }

    /// Empties the box, dropping the contents but preserving the allocation.
    pub fn empty(this: Self) -> OversizedBox<(), S> {
        Self::refill(this, ())
    }

    /// Empties a pinned box, dropping the contents but preserving the
    /// allocation.
    pub fn empty_pinned(this: Pin<Self>) -> OversizedBox<(), S> {
        // SAFETY: `empty` will just drop the current contents and not
        // otherwise access it. The pinned object has no more references, so it
        // does not violate any pin invariants to drop it and reuse the memory.
        Self::empty(unsafe { Pin::into_inner_unchecked(this) })
    }

    /// Pins the box.
    pub fn into_pin(this: Self) -> Pin<Self> {
        // SAFETY: the underlying object is allocated on the heap and will not
        // move until dropped.
        unsafe { Pin::new_unchecked(this) }
    }

    /// Consumes `this` and returns the allocation plus the phantom data
    /// specifying the allocation type.
    ///
    /// The phantom data is returned to make `coerce!` work.
    pub fn into_raw(this: Self) -> (NonNull<T>, PhantomData<*mut S>) {
        let Self { ptr, phantom } = this;
        std::mem::forget(this);
        (ptr, phantom)
    }

    /// Re-creates the box from the allocation plus phantom data specifying the
    /// underlying allocation type.
    ///
    /// The phantom data is consumed to make `coerce!` work.
    ///
    /// # Safety
    ///
    /// `t` must have been returned from `into_raw`.
    pub unsafe fn from_raw(t: NonNull<T>, s: PhantomData<*mut S>) -> Self {
        Self { ptr: t, phantom: s }
    }
}

/// Coerces an oversized box.
///
/// This is necessary because [`std::ops::CoerceUnsized`] is not stable.
///
/// # Example
///
/// ```rust
/// # use oversized_box::OversizedBox;
/// let x = OversizedBox::<_, u64>::new(5u32);
/// let y: OversizedBox<dyn Send, u64> = oversized_box::coerce!(x);
/// ```
///
/// You cannot use this to change the storage type. This will fail to build.
///
/// ```compile_fail
/// # use oversized_box::OversizedBox;
/// let x = OversizedBox::<_, u64>::new(5u32);
/// let y: OversizedBox<dyn Send, u32> = oversized_box::coerce!(x);
/// ```
#[macro_export]
macro_rules! coerce {
    ($e:expr) => {
        {
            let e: OversizedBox<_, _> = $e;
            let (t, s) = OversizedBox::into_raw(e);
            // SAFETY: This will coerce `t` and `s` using normal coercion rules.
            // Because `s` is a *mut S, it is invariant and will not coerce,
            // which is what we want. But `t` is a NonNull<T>, so it will coerce
            // to NonNull<T2> if T coerces to T2, which again is what we want.
            unsafe { OversizedBox::from_raw(t, s) }
        }
    };
}

impl<T: ?Sized, S> Drop for OversizedBox<T, S> {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` is owned and contains a T. But it's backed back a
        // Box<MaybeUninit<S>>.
        unsafe {
            std::ptr::drop_in_place(self.ptr.as_ptr());
            drop(Box::from_raw(self.ptr.as_ptr().cast::<MaybeUninit<S>>()));
        }
    }
}

impl<T: ?Sized, S> Deref for OversizedBox<T, S> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: ptr is valid for read.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: ?Sized, S> DerefMut for OversizedBox<T, S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: ptr is valid for write.
        unsafe { self.ptr.as_mut() }
    }
}

impl<T: ?Sized, S> AsRef<T> for OversizedBox<T, S> {
    fn as_ref(&self) -> &T {
        self
    }
}

impl<T: ?Sized, S> AsMut<T> for OversizedBox<T, S> {
    fn as_mut(&mut self) -> &mut T {
        self
    }
}

impl<T: ?Sized, S> From<OversizedBox<T, S>> for Pin<OversizedBox<T, S>> {
    fn from(this: OversizedBox<T, S>) -> Self {
        OversizedBox::into_pin(this)
    }
}

#[cfg(test)]
mod tests {
    use crate::OversizedBox;
    use std::fmt::Display;

    #[test]
    fn basic_test() {
        let x = OversizedBox::<_, [usize; 3]>::new(5u32);
        println!("{}", x.as_ref());
        let x = OversizedBox::refill(x, "now it's a string");
        println!("{}", x.as_ref());
        let x = OversizedBox::empty(x);
        let x = OversizedBox::refill(x, "string again");
        println!("{}", x.as_ref());
        let x: OversizedBox<dyn Display, _> = coerce!(x);
        println!("dyn {}", x.as_ref());
    }
}
