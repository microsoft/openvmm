// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides a mutex that can be closed for long-term access.
//!
//! This is useful if you have an object that is in one of two states: a
//! concurrent state, where it can be accessed by multiple users, and a
//! non-concurrent state, where it can only be accessed by one user.
//!
//! In the non-concurrent state, you can close the mutex guarding the object so
//! that it can be accessed freely without additional locking, allowing it to be
//! used in `async` functions (for example). When the object is to reenter the
//! concurrent state, you can open the mutex, allowing normal mutex operations.
//!
//! Something similar to this can be achieved with an ordinary mutex by holding
//! the lock for the lifetime of the non-concurrent state, but this means that
//! any other attempt to lock the mutex will hang for an indefinite period of
//! time, possibly deadlocking. `try_lock` cannot be used to overcome this,
//! because it would also fail while in the concurrent state with multiple
//! concurrent accessors competing for the lock.

// UNSAFETY: unsafe needed to implement interior mutability to locked values.
#![allow(unsafe_code)]
#![warn(missing_docs)]

use parking_lot::Mutex;
use parking_lot::MutexGuard;
use std::cell::UnsafeCell;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::ops::DerefMut;
use std::sync::Arc;

/// A mutex that can be _closed_.
///
/// A closed mutex can be accessed freely by the owner, but while closed it
/// cannot be locked by anyone else.
pub struct CloseableMutex<T: ?Sized> {
    mutex: Mutex<bool>,
    value: UnsafeCell<T>,
}

// SAFETY: `mutex` ensures that there is only a single concurrent access to
// `value`, providing `Sync` as long as `T` is `Send`.
unsafe impl<T: ?Sized + Send> Sync for CloseableMutex<T> {}

impl<T> CloseableMutex<T> {
    /// Returns a new instance wrapping the given value.
    pub fn new(value: T) -> Self {
        Self {
            mutex: Mutex::new(false),
            value: value.into(),
        }
    }
}

impl<T: ?Sized> CloseableMutex<T> {
    /// Closes the mutex, returning a guard that can be used to access the
    /// underlying value.
    ///
    /// When the guard is dropped, the mutex is re-opened.
    ///
    /// While the mutex is closed, calls to `lock_if_open` will return `None`,
    /// and calls to `lock` will panic.
    pub fn close(self: Arc<Self>) -> ClosedGuard<T> {
        {
            let mut closed = self.mutex.lock();
            assert!(!*closed, "object is already closed");
            *closed = true;
        }
        ClosedGuard(ManuallyDrop::new(self))
    }

    /// If the lock is open, waits for it to become available and returns a
    /// guard that can be used to access the underlying value.
    ///
    /// If the lock is closed, returns `None`.
    pub fn lock_if_open(&self) -> Option<OpenGuard<'_, T>> {
        let closed = self.mutex.lock();
        if *closed {
            return None;
        }
        MutexGuard::leak(closed);
        Some(OpenGuard(self))
    }

    /// Waits for the lock to become available and returns a guard that can be
    /// used to access the underlying value.
    ///
    /// # Panics
    /// Panics if the lock is closed. To avoid this, use `lock_if_open`.
    #[track_caller]
    pub fn lock(&self) -> OpenGuard<'_, T> {
        self.lock_if_open().expect("lock should not be closed")
    }
}

/// A guard that can be used to access the underlying value of a
/// [`CloseableMutex`].
#[must_use]
pub struct OpenGuard<'a, T: ?Sized>(&'a CloseableMutex<T>);

impl<T: ?Sized> Drop for OpenGuard<'_, T> {
    fn drop(&mut self) {
        // SAFETY: the mutex is known to be locked.
        unsafe {
            self.0.mutex.force_unlock();
        }
    }
}

impl<T: ?Sized> Deref for OpenGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: the mutex is known to be locked.
        unsafe { &*self.0.value.get() }
    }
}

impl<T: ?Sized> DerefMut for OpenGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: the mutex is known to be locked.
        unsafe { &mut *self.0.value.get() }
    }
}

/// A guard that can be used to access the underlying value of a
/// [`CloseableMutex`] while it is closed.
///
/// This wraps an [`Arc`] so that you can keep the mutex closed
/// for an unbounded period without having to deal with a lifetime.
// TODO: if this Arc-based functionality is not used or is otherwise
// inconvenient, then replace or augment this with a standard
// lifetime-based lock.
#[must_use]
pub struct ClosedGuard<T: ?Sized>(ManuallyDrop<Arc<CloseableMutex<T>>>);

impl<T: ?Sized> Drop for ClosedGuard<T> {
    fn drop(&mut self) {
        // SAFETY: this has not been called yet
        unsafe { self.release_ownership() };
    }
}

impl<T: ?Sized> ClosedGuard<T> {
    /// Opens the mutex, returning the inner instance.
    pub fn open(mut self) -> Arc<CloseableMutex<T>> {
        // SAFETY: this has not yet been called and will not be called again due
        // to the `forget`.
        let v = unsafe { self.release_ownership() };
        std::mem::forget(self);
        v
    }

    /// # Safety
    ///
    /// This must be called exactly once.
    unsafe fn release_ownership(&mut self) -> Arc<CloseableMutex<T>> {
        let was_owned = std::mem::replace(&mut *self.0.mutex.lock(), false);
        assert!(was_owned);
        // SAFETY: this is called exactly once.
        unsafe { ManuallyDrop::take(&mut self.0) }
    }
}

impl<T: ?Sized> Deref for ClosedGuard<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: the mutex is known to be closed.
        unsafe { &*self.0.value.get() }
    }
}

impl<T: ?Sized> DerefMut for ClosedGuard<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: the mutex is known to be closed.
        unsafe { &mut *self.0.value.get() }
    }
}

#[cfg(test)]
mod tests {
    use crate::CloseableMutex;
    use std::sync::Arc;

    #[test]
    fn test_mutex() {
        let x = Arc::new(CloseableMutex::new(0));
        *x.lock() = 5;
        *x.lock() = 6;
        assert_eq!(*x.lock(), 6);

        // Close the mutex, make sure locks are disallowed.
        {
            let mut c = x.clone().close();
            *c = 7;
            assert!(x.lock_if_open().is_none());
        }

        // Locks are allowed again.
        assert_eq!(*x.lock_if_open().unwrap(), 7);
        assert_eq!(*x.lock(), 7);
    }

    #[test]
    #[should_panic]
    fn test_closed_mutex_panics() {
        let x = Arc::new(CloseableMutex::new(0));
        let _c = x.clone().close();
        let _ = x.lock();
    }
}
