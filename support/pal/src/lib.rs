// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This crate provides a set of types that abstract over OS-specific platform
//! primitives. It is focused on IO- and wait-related functionality: events,
//! timers, and polling.
//!
//! As a convenience, it also exports some OS-specific functionality and some
//! general library functionality.

#![expect(missing_docs)]

pub mod process;
pub mod unix;
pub mod windows;

// Re-export windows_sys and windows_result so that the delayload! macro works
// when used in other crates
#[cfg(windows)]
pub use windows_result;
#[cfg(windows)]
pub use windows_sys;

pub use sys::close_stdout;
pub use sys::pipe::pair as pipe_pair;

#[cfg(unix)]
use unix as sys;
#[cfg(windows)]
use windows as sys;

/// Runs a closure when the instance goes out of scope.
pub struct ScopeExit<F: FnOnce()> {
    func: Option<F>,
}

impl<F: FnOnce()> ScopeExit<F> {
    /// Creates a new `ScopeExit`.
    pub fn new(func: F) -> Self {
        Self { func: Some(func) }
    }

    /// Prevents the closure from running when the instance goes out of scope.
    ///
    /// This function takes ownership so that any variables borrowed by the closure will be
    /// usable again after calling cancel.
    pub fn dismiss(mut self) {
        self.func = None;
    }
}

impl<F: FnOnce()> Drop for ScopeExit<F> {
    fn drop(&mut self) {
        if let Some(func) = self.func.take() {
            func();
        }
    }
}
