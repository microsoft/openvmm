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
#![expect(unsafe_code)]

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
    mutex: Mutex<MutexState>,
    value: UnsafeCell<ManuallyDrop<T>>,
}

struct MutexState {
    closed: bool,
    taken: bool,
}

impl<T: ?Sized> Drop for CloseableMutex<T> {
    fn drop(&mut self) {
        if !self.mutex.get_mut().taken {
            // SAFETY: the last reference is gone and the value has not been taken.
            unsafe { ManuallyDrop::drop(self.value.get_mut()) };
        }
    }
}

// SAFETY: `mutex` ensures that there is only a single concurrent access to
// `value`, providing `Sync` as long as `T` is `Send`.
unsafe impl<T: ?Sized + Send> Sync for CloseableMutex<T> {}

impl<T> CloseableMutex<T> {
    /// Returns a new instance wrapping the given value.
    pub fn new(value: T) -> Self {
        Self {
            mutex: Mutex::new(MutexState {
                closed: false,
                taken: false,
            }),
            value: UnsafeCell::new(ManuallyDrop::new(value)),
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
            assert!(!closed.closed, "object is already closed");
            closed.closed = true;
        }
        ClosedGuard {
            mutex: ManuallyDrop::new(self),
            reopen: true,
        }
    }

    /// Closes the mutex permanently, returning exclusive access to its value.
    ///
    /// Other references cannot lock the mutex, even after this guard is dropped.
    /// The returned guard cannot reopen the mutex.
    pub fn close_permanently(self: Arc<Self>) -> PermanentClosedGuard<T> {
        let mut guard = self.close();
        guard.reopen = false;
        PermanentClosedGuard(guard)
    }

    /// If the lock is open, waits for it to become available and returns a
    /// guard that can be used to access the underlying value.
    ///
    /// If the lock is closed, returns `None`.
    pub fn lock_if_open(&self) -> Option<OpenGuard<'_, T>> {
        let closed = self.mutex.lock();
        if closed.closed {
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
pub struct ClosedGuard<T: ?Sized> {
    mutex: ManuallyDrop<Arc<CloseableMutex<T>>>,
    reopen: bool,
}

impl<T: ?Sized> Drop for ClosedGuard<T> {
    fn drop(&mut self) {
        // SAFETY: this has not been called yet
        unsafe { self.release_ownership() };
    }
}

/// Exclusive access to a permanently closed mutex.
///
/// Unlike [`ClosedGuard`], this guard cannot reopen the mutex.
#[must_use]
pub struct PermanentClosedGuard<T: ?Sized>(ClosedGuard<T>);

impl<T> PermanentClosedGuard<T> {
    /// Takes the value while leaving all remaining references permanently closed.
    ///
    pub fn take(self) -> T {
        {
            let mut state = self.0.mutex.mutex.lock();
            assert!(!state.taken);
            state.taken = true;
        }
        // SAFETY: this guard has exclusive access, cannot reopen the mutex, and
        // records extraction before moving the value to prevent a double drop.
        unsafe { ManuallyDrop::take(&mut *self.0.mutex.value.get()) }
    }
}

impl<T: ?Sized> Deref for PermanentClosedGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: ?Sized> DerefMut for PermanentClosedGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<T: ?Sized> ClosedGuard<T> {
    /// Opens the mutex, returning the inner instance.
    ///
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
        {
            let mut closed = self.mutex.mutex.lock();
            assert!(closed.closed);
            if self.reopen {
                closed.closed = false;
            }
        }
        // SAFETY: this is called exactly once.
        unsafe { ManuallyDrop::take(&mut self.mutex) }
    }
}

impl<T: ?Sized> Deref for ClosedGuard<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: the mutex is known to be closed.
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T: ?Sized> DerefMut for ClosedGuard<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: the mutex is known to be closed.
        unsafe { &mut *self.mutex.value.get() }
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

    #[test]
    fn permanently_closed_mutex_stays_closed() {
        let x = Arc::new(CloseableMutex::new(0));
        let mut closed = x.clone().close_permanently();
        *closed = 1;
        assert!(x.lock_if_open().is_none());
        drop(closed);
        assert!(x.lock_if_open().is_none());
    }

    #[test]
    fn take_transfers_value_despite_retained_references() {
        #[derive(Debug)]
        struct CountDrop(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for CountDrop {
            fn drop(&mut self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let value = Arc::new(CloseableMutex::new(CountDrop(count.clone())));
        let erased: Arc<CloseableMutex<dyn std::fmt::Debug + Send>> = value.clone();
        let extracted = value.clone().close_permanently().take();
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(value.lock_if_open().is_none());
        assert!(erased.lock_if_open().is_none());
        drop(extracted);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(value.lock_if_open().is_none());
        drop(value);
        drop(erased);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn panicking_destructor_is_not_called_twice() {
        struct PanicDrop(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for PanicDrop {
            fn drop(&mut self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                panic!("destructor failed");
            }
        }
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let value = Arc::new(CloseableMutex::new(PanicDrop(count.clone())));
        let closed = value.clone().close_permanently();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(closed.take()))).is_err()
        );
        assert!(value.lock_if_open().is_none());
        drop(value);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
