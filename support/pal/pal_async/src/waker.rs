// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Utilities for wakers.

use std::task::Waker;

/// A list of wakers that are ready to be woken.
#[must_use]
#[derive(Debug, Default)]
pub struct WakerList(Vec<Waker>);

impl WakerList {
    /// Wakes all the wakers.
    pub fn wake(&mut self) {
        for waker in self.0.drain(..) {
            waker.wake();
        }
    }

    #[expect(dead_code)]
    pub fn push(&mut self, waker: Waker) {
        self.0.push(waker);
    }
}

impl Extend<Waker> for WakerList {
    fn extend<T: IntoIterator<Item = Waker>>(&mut self, iter: T) {
        self.0.extend(iter);
    }
}

impl FromIterator<Waker> for WakerList {
    fn from_iter<T: IntoIterator<Item = Waker>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}
