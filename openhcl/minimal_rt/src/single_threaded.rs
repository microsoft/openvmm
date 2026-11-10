// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for global variables in a single-threaded environment.

use core::ops::Deref;

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
