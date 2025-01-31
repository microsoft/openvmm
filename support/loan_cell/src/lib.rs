// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This crate provides a [`LoanCell`] type that allows for lending a reference
//! to a value for a limited scope.
//!
//! This is useful for publishing a reference to an on-stack value into a
//! thread-local variable for the lifetime of a function call. This can be
//! useful when you can't or don't want to temporarily move the value to the
//! thread-local variable:
//!   - The value is not sized (e.g., it is a slice or a dyn trait object), so
//!     you can't move it.
//!   - The value is large, so it would be inefficient to move it.
//!   - The value has a destructor, so putting it in TLS would use the
//!     inefficient form of Rust TLS that registers a destructor for the value.
//!
//! [`LoanCell`] is not `Sync` or `Send`, so it can only be used within a single
//! thread. This is necessary to ensure that the loaned value is not accessed
//! after the function that loaned it returns.
//!
//! # Example
//!
//! ```rust
//! use loan_cell::LoanCell;
//!
//! thread_local! {
//!    static CONTEXT: LoanCell<str> = const { LoanCell::new() };
//! }
//!
//! fn print_name() -> String {
//!     CONTEXT.with(|name| {
//!         name.borrow(|name| {
//!             format!("stored {}", name.unwrap_or("nowhere"))
//!         })
//!     })
//! }
//!
//! CONTEXT.with(|v| {
//!     assert_eq!(v.lend(&String::from("in the heap"), || print_name()), "stored in the heap");
//!     assert_eq!(v.lend("statically", || print_name()), "stored statically");
//! });
//! assert_eq!(print_name(), "stored nowhere");
//! ```

// UNSAFETY: this is needed to work around the borrow checker.
#![allow(unsafe_code)]
#![warn(missing_docs)]
#![no_std]

use core::cell::Cell;
use core::panic::RefUnwindSafe;
use core::panic::UnwindSafe;
use core::ptr::NonNull;

/// A cell that allows lending a reference to a value for a limited scope.
///
/// See the [module-level documentation](crate) for more information.
#[derive(Default)]
pub struct LoanCell<T: ?Sized>(Cell<Option<NonNull<T>>>);

impl<T: RefUnwindSafe + ?Sized> UnwindSafe for LoanCell<T> {}
impl<T: RefUnwindSafe + ?Sized> RefUnwindSafe for LoanCell<T> {}

impl<T: ?Sized> LoanCell<T> {
    /// Creates a `LoanCell` with no loaned data.
    pub const fn new() -> Self {
        Self(Cell::new(None))
    }

    /// Lends `value` for the lifetime of `f`. `f` or any function it calls can
    /// access the loaned value via [`LoanCell::borrow`].
    ///
    /// If a value is already lent, it is replaced with `value` for the duration
    /// of `f` and restored afterwards.
    pub fn lend<R>(&self, value: &T, f: impl FnOnce() -> R) -> R {
        // Use a guard to restore the old value after `f` returns or panics.
        struct RestoreOnDrop<'a, T: ?Sized>(&'a LoanCell<T>, Option<NonNull<T>>);
        impl<T: ?Sized> Drop for RestoreOnDrop<'_, T> {
            fn drop(&mut self) {
                self.0 .0.set(self.1);
            }
        }

        let old = self.0.replace(Some(value.into()));
        let _guard = RestoreOnDrop(self, old);
        f()
    }

    /// Returns `true` if a value is currently lent.
    pub fn is_lent(&self) -> bool {
        self.0.get().is_some()
    }

    /// Borrows the lent value for the duration of `f`. If no value is currently
    /// lent, `f` is called with `None`.
    pub fn borrow<R>(&self, f: impl FnOnce(Option<&T>) -> R) -> R {
        let v = self.0.get().map(|v| {
            // SAFETY: the inner value is alive as long as the corresponding
            // `lend` call is running, and the value is only dropped when the
            // function passed to `lend` returns. Since `LoanCell` is not
            // `Sync`, and so `lend` must be running on the same thread as this
            // call, this cannot happen while `f` is running.
            unsafe { v.as_ref() }
        });
        f(v)
    }
}

#[cfg(test)]
mod tests {
    use super::LoanCell;

    extern crate std;

    static_assertions::assert_not_impl_any!(LoanCell<()>: Sync, Send);

    #[test]
    fn loan() {
        struct NoCopy<T>(T);
        let cell = LoanCell::new();
        cell.borrow(|v| assert!(v.is_none()));
        let result = cell.lend(&NoCopy(42), || {
            cell.borrow(|v| {
                assert_eq!(v.unwrap().0, 42);
            });
            42
        });
        assert_eq!(result, 42);
        cell.borrow(|v| assert!(v.is_none()));
    }

    #[test]
    fn nested_loan() {
        let cell = LoanCell::new();
        cell.lend(&42, || {
            cell.lend(&52, || {
                cell.borrow(|v| {
                    assert_eq!(v.unwrap(), &52);
                });
            });
            cell.borrow(|v| {
                assert_eq!(v.unwrap(), &42);
            });
        });
    }

    #[test]
    fn unsized_loan() {
        let cell = LoanCell::new();
        let value = "hello";
        cell.lend(value, || {
            cell.borrow(|v| {
                assert_eq!(v.unwrap(), value);
            });
        });
    }

    #[test]
    fn panicked_loan() {
        let cell = LoanCell::new();
        let result = std::panic::catch_unwind(|| {
            cell.lend(&42, || {
                panic!();
            });
        });
        assert!(result.is_err());
        cell.borrow(|v| assert!(v.is_none()));
    }
}
