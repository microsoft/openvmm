// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: Calling nice.
#![expect(unsafe_code)]

pub(crate) fn nice(i: i32) {
    // SAFETY: calling as documented.
    unsafe {
        libc::nice(i);
    }
}
