// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A wrapper around AtomicPtr that automatically adds a blackbox hint to
//! prevent it from being optimized out. Intended to be used as a write-
//! only pointer used to easily find interested variables when debugging.

#![forbid(unsafe_code)]

use std::sync::atomic::AtomicPtr;
use std::sync::atomic::Ordering;

/// A pointer wrapper for debugging purposes
pub struct DebugPtr<T>(AtomicPtr<T>);

impl<T> DebugPtr<T> {
    /// Creates a new debug pointer, initialized to null.
    pub const fn new() -> Self {
        DebugPtr(AtomicPtr::new(std::ptr::null_mut()))
    }

    /// Stores the provided reference as an AtomicPtr and uses a blackbox
    /// hint to prevent it from being optimized out.
    pub fn store(&self, ptr: &T) {
        self.0
            .store(std::ptr::from_ref(ptr).cast_mut(), Ordering::Relaxed);
        std::hint::black_box(&self.0);
    }
}
