// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ops::Deref;

// Similar to Cow, but doesn't require T to implement ToOwned.
pub enum BorrowedOrOwned<'a, T> {
    Borrowed(&'a T),
    Owned(T),
}

impl<T> Deref for BorrowedOrOwned<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        match self {
            BorrowedOrOwned::Borrowed(t) => t,
            BorrowedOrOwned::Owned(t) => t,
        }
    }
}
