// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// Trait for reference time.
pub trait ReferenceTimeSource: Send + Sync {
    /// Returns the current time in 100ns units.
    fn now_100ns(&self) -> u64;
    /// Returns if this reference time is backed by TSC.
    fn is_backed_by_tsc(&self) -> bool;
}
