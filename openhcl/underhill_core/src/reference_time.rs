// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Definitions for Microsoft Hypervisor reference time.

use inspect::Inspect;
use std::time::Duration;

/// A hypervisor reference time value. This is a 64-bit value that starts at 0
/// when the VM boots (typically) and measures elapsed time in 100ns units.
///
/// It may stop while the VM is paused.
#[derive(Copy, Clone, Debug, Inspect)]
#[inspect(transparent(hex))]
pub struct ReferenceTime(u64);

impl ReferenceTime {
    /// Wraps a reference time value.
    pub fn new(value_100ns: u64) -> Self {
        Self(value_100ns)
    }

    /// Returns the reference time in 100ns units.
    pub fn as_100ns(&self) -> u64 {
        self.0
    }

    /// Computes the change in reference time since `start`.
    ///
    /// Returns `None` if `start` is after `self`.
    pub fn since(&self, start: ReferenceTime) -> Option<Duration> {
        let diff_100ns = self.0.wrapping_sub(start.0);
        if (diff_100ns as i64) < 0 {
            return None;
        }
        // Can't just use from_nanos since that could overflow.
        let count_per_sec = 10 * 1000 * 1000;
        Some(Duration::new(
            diff_100ns / count_per_sec,
            (diff_100ns % count_per_sec) as u32 * 100,
        ))
    }
}
