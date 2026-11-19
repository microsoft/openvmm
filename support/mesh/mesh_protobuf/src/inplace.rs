// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides an `Option`-like type for constructing values in place.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::mem::MaybeUninit;

/// A type with methods like `Option` but that operates on a mutable reference
/// to possibly-initialized data.
///
/// This is used to initialize data in place without copying to/from `Option`
/// types.
pub struct InplaceOption<'a, T> {
    val: &'a mut MaybeUninit<T>,
    init: bool,
}

impl<'a, T> InplaceOption<'a, T> {
    /// Creates an option in the uninitialized state.
    pub fn uninit(val: &'a mut MaybeUninit<T>) -> Self {
        Self { val, init: false }
    }

    /// Creates an option in the initialized state.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that the value referenced by `val` is
    /// initialized.
    pub unsafe fn new_init_unchecked(val: &'a mut MaybeUninit<T>) -> Self {
        Self { val, init: true }
    }

    /// Sets the value to the initialized state.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that the underlying data has been fully
    /// initialized.
    pub unsafe fn set_init_unchecked(&mut self) -> &mut T {
        self.init = true;
        // SAFETY: the caller guarantees val is initialized.
        unsafe { self.val.assume_init_mut() }
    }

    /// Takes the value, returning `Some(_)` if the value is initialized and
    /// `None` otherwise.
    pub fn take(&mut self) -> Option<T> {
        if self.init {
            self.init = false;
            // SAFETY: val is initialized
            Some(unsafe { self.val.assume_init_read() })
        } else {
            None
        }
    }

    /// Returns a reference to the data if it's initialized.
    pub fn as_ref(&self) -> Option<&T> {
        if self.init {
            // SAFETY: We have just checked that val is initialized
            unsafe { self.val.as_ptr().as_ref() }
        } else {
            None
        }
    }

    /// Returns a mutable reference to the data if it's initialized.
    pub fn as_mut(&mut self) -> Option<&mut T> {
        if self.init {
            // SAFETY: val is initialized
            Some(unsafe { self.val.assume_init_mut() })
        } else {
            None
        }
    }

    /// Clears the data to the uninitialized state.
    pub fn clear(&mut self) {
        if self.init {
            self.init = false;
            // SAFETY: val is initialized
            unsafe { self.val.assume_init_drop() };
        }
    }

    /// Resets the data to the uninitialized state without dropping any
    /// initialized value.
    pub fn forget(&mut self) -> bool {
        core::mem::take(&mut self.init)
    }

    /// Initializes the value to `v`, dropping any existing value first.
    pub fn set(&mut self, v: T) -> &mut T {
        self.clear();
        self.init = true;
        self.val.write(v)
    }

    /// Gets a mutable reference to the value, setting it to `v` first if it's
    /// not initialized.
    pub fn get_or_insert(&mut self, v: T) -> &mut T {
        self.get_or_insert_with(|| v)
    }

    /// Gets a mutable reference to the value, setting it to `f()` first if it's
    /// not initialized.
    pub fn get_or_insert_with(&mut self, f: impl FnOnce() -> T) -> &mut T {
        if self.init {
            // SAFETY: val is initialized
            unsafe { self.val.assume_init_mut() }
        } else {
            self.init = true;
            self.val.write(f())
        }
    }

    /// Returns whether the value is initialized.
    pub fn is_some(&self) -> bool {
        self.init
    }

    /// Returns whether the value is uninitialized.
    pub fn is_none(&self) -> bool {
        !self.init
    }

    /// Returns a const pointer to the underlying value (initialized or not).
    pub fn as_ptr(&self) -> *const T {
        self.val.as_ptr()
    }

    /// Returns a mut pointer to the underlying value (initialized or not).
    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.val.as_mut_ptr()
    }
}

impl<T> InplaceOption<'_, Box<T>> {
    /// Updates a boxed value in place.
    ///
    /// N.B. This will allocate space for a value if one is not already present,
    ///      which is wasteful if `f` does not actually initialize the value.
    pub fn update_box<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut InplaceOption<'_, T>) -> R,
    {
        let mut boxed;
        let mut inplace;

        if let Some(b) = self.take() {
            // SAFETY: MaybeUninit<T> has the same layout as T.
            boxed = unsafe { Box::from_raw(Box::into_raw(b).cast::<MaybeUninit<T>>()) };
            // SAFETY: the value is known to be initialized.
            inplace = unsafe { InplaceOption::new_init_unchecked(&mut *boxed) };
        } else {
            boxed = Box::new(MaybeUninit::uninit());
            inplace = InplaceOption::uninit(&mut *boxed);
        }

        let r = f(&mut inplace);
        if inplace.forget() {
            drop(inplace);
            // SAFETY: the value is known to be initialized.
            let b = unsafe { boxed.assume_init() };
            self.set(b);
        }
        r
    }
}

impl<T: Clone> InplaceOption<'_, Arc<T>> {
    /// Updates a reference counted value in place.
    ///
    /// N.B. This will allocate space for a value if one is not already present,
    ///      which is wasteful if `f` does not actually initialize the value.
    pub fn update_arc<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut InplaceOption<'_, T>) -> R,
    {
        let mut arced;
        let mut inplace;

        if let Some(mut a) = self.take() {
            // Ensure there is only a single reference.
            Arc::make_mut(&mut a);
            // SAFETY: MaybeUninit<T> has the same layout as T.
            arced = unsafe { Arc::from_raw(Arc::into_raw(a).cast::<MaybeUninit<T>>()) };
            // SAFETY: the value is known to be initialized.
            unsafe {
                inplace = InplaceOption::new_init_unchecked(Arc::get_mut(&mut arced).unwrap())
            };
        } else {
            arced = Arc::new(MaybeUninit::uninit());
            inplace = InplaceOption::uninit(Arc::get_mut(&mut arced).unwrap());
        }

        let r = f(&mut inplace);
        if inplace.forget() {
            drop(inplace);
            // SAFETY: the value is known to be initialized.
            let a = unsafe { arced.assume_init() };
            self.set(a);
        }
        r
    }
}

impl<T> Drop for InplaceOption<'_, T> {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Constructs a possibly-initialized [`crate::inplace::InplaceOption`] on the stack
/// from an `Option<T>`.
#[macro_export]
macro_rules! inplace {
    ($v:ident) => {
        let opt = $v;
        let mut $v;
        let mut $v = match opt {
            Some(v) => {
                $v = core::mem::MaybeUninit::new(v);
                // SAFETY: We just initialized the value.
                unsafe { $crate::inplace::InplaceOption::new_init_unchecked(&mut $v) }
            }
            None => {
                $v = core::mem::MaybeUninit::uninit();
                $crate::inplace::InplaceOption::uninit(&mut $v)
            }
        };
    };
}

/// Constructs an initialized [`crate::inplace::InplaceOption`] on the stack from a `T`.
#[macro_export]
macro_rules! inplace_some {
    ($v:ident) => {
        let mut $v = core::mem::MaybeUninit::new($v);
        // SAFETY: We just initialized the value.
        let mut $v = unsafe { $crate::inplace::InplaceOption::new_init_unchecked(&mut $v) };
    };
}

/// Constructs an uninitialized [`crate::inplace::InplaceOption`] on the stack.
#[macro_export]
macro_rules! inplace_none {
    ($v:ident) => {
        let mut $v = core::mem::MaybeUninit::uninit();
        let mut $v = $crate::inplace::InplaceOption::uninit(&mut $v);
    };
    ($v:ident : $t:ty) => {
        let mut $v = core::mem::MaybeUninit::<$t>::uninit();
        let mut $v = $crate::inplace::InplaceOption::uninit(&mut $v);
    };
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::string::ToString;
    use alloc::sync::Arc;
    use test_with_tracing::test;

    #[test]
    fn test_inplace_some() {
        let v = "test".to_string();
        inplace_some!(v);
        assert_eq!(&v.take().unwrap(), "test");
        assert!(v.is_none());
        assert!(v.take().is_none());
    }

    #[test]
    fn test_inplace_none() {
        inplace_none!(v: String);
        v.set("test".to_string());
        assert_eq!(&v.take().unwrap(), "test");
    }

    #[test]
    fn test_inplace() {
        let v = Some("test".to_string());
        inplace!(v);
        assert_eq!(&v.take().unwrap(), "test");
    }

    #[test]
    fn test_inplace_replace() {
        let v = "old".to_string();
        inplace_some!(v);
        v.set("new".to_string());
        assert_eq!(&v.take().unwrap(), "new");
    }

    #[test]
    fn test_updates() {
        let v = Arc::new(Box::new(1234));
        let arc_ptr = Arc::as_ptr(&v);
        let box_ptr = core::ptr::from_ref(&**v);
        inplace_some!(v);
        v.update_arc(|v| {
            v.update_box(|v| {
                v.set(5678);
            });
        });
        let v = v.take().unwrap();
        assert_eq!(**v, 5678);
        assert_eq!(Arc::as_ptr(&v), arc_ptr);
        assert_eq!(core::ptr::from_ref(&**v), box_ptr);
    }

    #[test]
    fn test_take_transfers_ownership_once() {
        let value = Arc::new(1234);
        let v = value.clone();
        inplace_some!(v);
        let taken = v.take().unwrap();
        assert!(v.take().is_none());
        drop(v);
        assert_eq!(Arc::strong_count(&value), 2);
        drop(taken);
        assert_eq!(Arc::strong_count(&value), 1);
    }

    #[test]
    fn test_updates_uninitialized() {
        inplace_none!(boxed: Box<u32>);
        boxed.update_box(|v| {
            assert!(v.is_none());
            v.set(1234);
        });
        assert_eq!(*boxed.take().unwrap(), 1234);

        inplace_none!(arced: Arc<u32>);
        arced.update_arc(|v| {
            assert!(v.is_none());
            v.set(5678);
        });
        assert_eq!(*arced.take().unwrap(), 5678);
    }

    #[test]
    fn test_updates_clear_values() {
        let value = Arc::new(1234);
        let boxed = Box::new(value.clone());
        inplace_some!(boxed);
        boxed.update_box(|v| v.clear());
        assert!(boxed.is_none());
        assert_eq!(Arc::strong_count(&value), 1);

        let arced = Arc::new(value.clone());
        inplace_some!(arced);
        arced.update_arc(|v| v.clear());
        assert!(arced.is_none());
        assert_eq!(Arc::strong_count(&value), 1);
    }

    #[test]
    fn test_update_arc_preserves_shared_value() {
        let original = Arc::new(1234);
        let v = original.clone();
        inplace_some!(v);
        v.update_arc(|v| {
            *v.as_mut().unwrap() = 5678;
        });
        let v = v.take().unwrap();
        assert_eq!(*original, 1234);
        assert_eq!(*v, 5678);
        assert!(!Arc::ptr_eq(&original, &v));
    }
}
