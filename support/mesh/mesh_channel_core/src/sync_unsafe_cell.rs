// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: needed to implement `Sync` for `SyncUnsafeCell`.
#![expect(unsafe_code)]

use std::cell::UnsafeCell;
use std::fmt::Debug;

/// A wrapper around [`UnsafeCell`] that is [`Sync`] when the inner type is
/// [`Sync`].
///
/// Replace with `std::cell::SyncUnsafeCell` when stabilized.
#[derive(Default)]
pub(crate) struct SyncUnsafeCell<T>(pub UnsafeCell<T>);

impl<T> Debug for SyncUnsafeCell<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&self.0, f)
    }
}

impl<T> SyncUnsafeCell<T> {
    pub fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }
}

// Replace with `std::cell::SyncUnsafeCell` when stabilized.
//
// SAFETY: `UnsafeCell` is not inherently `!Sync`, but it is explicitly `!Sync`
// to prevent bugs due to interior mutability across threads.
unsafe impl<T: Sync> Sync for SyncUnsafeCell<T> {}
