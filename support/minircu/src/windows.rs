// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use windows_sys::Win32::System::Threading::FlushProcessWriteBuffers;

// Use a compiler fence on the read side since we have a working membarrier
// implementation.
pub use std::sync::atomic::compiler_fence as access_fence;

pub fn membarrier() {
    // Use the FlushProcessWriteBuffers function to ensure that all other threads in the process
    // have observed the writes made by this thread.

    // SAFETY: no special requirements for the call.
    unsafe { FlushProcessWriteBuffers() }
}
