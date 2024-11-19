// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypervisor reference time.

/// Returns the current reference time from the hypervisor, in 100ns units.
pub fn reference_time() -> u64 {
    crate::arch::reference_time()
}
