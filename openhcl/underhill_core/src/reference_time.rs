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
        Some(Duration::from_nanos_u128(u128::from(diff_100ns) * 100))
    }
}

#[cfg(test)]
mod tests {
    use super::ReferenceTime;
    use std::time::Duration;
    use test_with_tracing::test;

    #[test]
    fn since_preserves_large_durations_and_wrapping_order() {
        for ticks in [0, 1, 9_999_999, 10_000_000, i64::MAX as u64] {
            let duration = ReferenceTime::new(ticks)
                .since(ReferenceTime::new(0))
                .unwrap();
            assert_eq!(duration.as_nanos(), u128::from(ticks) * 100);
        }
        assert_eq!(
            ReferenceTime::new(0).since(ReferenceTime::new(u64::MAX)),
            Some(Duration::from_nanos(100)),
        );
        assert_eq!(ReferenceTime::new(0).since(ReferenceTime::new(1)), None);
        assert_eq!(
            ReferenceTime::new(1 << 63).since(ReferenceTime::new(0)),
            None,
        );
    }
}
