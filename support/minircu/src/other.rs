// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Use a memory barrier on the read side since we don't have a working
// membarrier implementation to force a barrier remotely from the write side.
pub use std::sync::atomic::fence as access_fence;

pub fn membarrier() {
    // No suitable implementation on this platform.
}
