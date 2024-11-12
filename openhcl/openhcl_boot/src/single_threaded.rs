// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for working with global variables in a single-threaded environment.
//! In such an environment, it is safe to access globals even if they don't
//! implement [`Sync`], since there is only one thread that can access them. But
//! code still needs to be careful to avoid creating multiple _mutable_
//! references to the same global. These types provide abstractions for doing
//! this safely.

use core::cell::Cell;
use core::cell::UnsafeCell;
use core::ops::Deref;
use core::ops::DerefMut;

/// A wrapper around a value that implements `Sync` even if `T` does not
/// implement `Sync`.
///
/// This is only safe to use in a single-threaded environment. Do not compile
/// this type into a multi-threaded environment.
pub struct SingleThreaded<T>(pub T);

// SAFETY: we must mark this as Sync so that it can be `static`. It is
// not actually necessarily Sync, so this can only be used in a
// single-threaded environment.
unsafe impl<T> Sync for SingleThreaded<T> {}

impl<T> Deref for SingleThreaded<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

/// A reference returned by [`off_stack`].
pub struct OffStackRef<'a, T>(&'a mut T, BorrowRef<'a>);

impl<'a, T> OffStackRef<'a, T> {
    #[track_caller]
    #[doc(hidden)]
    pub unsafe fn new_internal(value: &'a UnsafeCell<T>, used: &'a Cell<bool>) -> Self {
        let r = BorrowRef::try_new(used).expect("function recursed");
        // SAFETY: we just set `used` to true, so we know that we are the only
        // one accessing `value`.
        let value = unsafe { &mut *value.get() };
        OffStackRef(value, r)
    }

    /// Leaks the borrow, returning the reference.
    ///
    /// This will lead to a panic if there is an attempt to borrow the value
    /// again (e.g., if the function invoking the `off_stack` macro is called
    /// again).
    pub fn leak(this: Self) -> &'a mut T {
        core::mem::forget(this.1);
        this.0
    }
}

struct BorrowRef<'a>(&'a Cell<bool>);

impl<'a> BorrowRef<'a> {
    fn try_new(used: &'a Cell<bool>) -> Option<Self> {
        if used.replace(true) {
            None
        } else {
            Some(Self(used))
        }
    }
}

impl Drop for BorrowRef<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

impl<T> Deref for OffStackRef<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.0
    }
}

impl<T> DerefMut for OffStackRef<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.0
    }
}

/// Returns a mutable reference to a value that is stored as a global `static`
/// variable rather than exist on the stack.
///
/// This is useful for working with large objects that don't fit on the stack.
/// It is an alternative to using [`SingleThreaded`] with
/// [`RefCell`](core::cell::RefCell); `RefCell` has the disadvantage of putting
/// an extra `bool` next to the value in memory, which can waste a lot of space
/// for heavily-aligned objects.
///
/// Panics if this function is called recursively, since this would attempt to
/// create multiple mutable references to the same global variable.
///
/// This only works in a single-threaded environment.
macro_rules! off_stack {
    ($ty:ty, $val:expr) => {{
        use core::cell::Cell;
        use core::cell::UnsafeCell;
        use $crate::single_threaded::OffStackRef;
        use $crate::single_threaded::SingleThreaded;

        static VALUE: SingleThreaded<UnsafeCell<$ty>> = SingleThreaded(UnsafeCell::new($val));
        static USED: SingleThreaded<Cell<bool>> = SingleThreaded(Cell::new(false));

        // SAFETY: `USED` is always used to track the usage of `VALUE`.
        unsafe { OffStackRef::new_internal(&VALUE.0, &USED.0) }
    }};
}
pub(crate) use off_stack;
