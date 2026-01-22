// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Some useful built-in implementations of `LocalClock`.

use super::LocalClock;
use super::LocalClockDelta;
use super::LocalClockTime;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::SystemTime;

/// An implementation of [`LocalClock`] backed by [`std::time::SystemTime`].
///
/// # A note on Time Travel
///
/// The time reported by [`std::time::SystemTime`] may be radically altered by
/// external factors, such as the system operator manually setting the global
/// system clock. If this happens, `SystemTimeClock` will _also_ jump
/// forwards/backwards in time in-tandem, depending on how far the clock was
/// rolled back/forwards!
///
/// If this is something that concerns you, you might want to consider writing a
/// custom [`LocalClock`] implementation backed by something akin to Linux's
/// `CLOCK_BOOTTIME`, which provides a stable monotonically increasing clock
/// resilient against host suspends / resumes, and not subject to unexpected
/// negative / positive time jumps.
#[derive(Debug)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
pub struct SystemTimeClock {
    offset_from_system_time: LocalClockDelta,
}

impl SystemTimeClock {
    /// Create a new [`SystemTimeClock`], set to the current [`SystemTime`] plus
    /// `initial_delta`.
    pub fn new(initial_delta: LocalClockDelta) -> SystemTimeClock {
        SystemTimeClock {
            offset_from_system_time: initial_delta,
        }
    }
}

impl LocalClock for SystemTimeClock {
    fn get_time(&mut self) -> LocalClockTime {
        LocalClockTime::from(SystemTime::now()) + self.offset_from_system_time
    }

    fn set_time(&mut self, new_time: LocalClockTime) {
        self.offset_from_system_time = new_time - LocalClockTime::from(SystemTime::now());
    }
}

/// A mock implementation of [`LocalClock`], which is manually ticked via a
/// [`MockLocalClockAccessor`]. Useful for tests.
#[derive(Debug)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
pub struct MockLocalClock {
    time: Arc<Mutex<LocalClockTime>>,
}

impl MockLocalClock {
    /// Create a new [`MockLocalClock`]
    pub fn new() -> Self {
        MockLocalClock {
            time: Arc::new(Mutex::new(LocalClockTime::from_millis_since_unix_epoch(
                1337,
            ))),
        }
    }

    /// Return a new [`MockLocalClockAccessor`], which can tick the clock.
    pub fn accessor(&self) -> MockLocalClockAccessor {
        MockLocalClockAccessor {
            time: self.time.clone(),
        }
    }
}

impl LocalClock for MockLocalClock {
    fn get_time(&mut self) -> LocalClockTime {
        *self.time.lock()
    }

    fn set_time(&mut self, new_time: LocalClockTime) {
        *self.time.lock() = new_time
    }
}

/// Handle to manually tick an instance of [`MockLocalClock`].
#[derive(Clone)]
pub struct MockLocalClockAccessor {
    time: Arc<Mutex<LocalClockTime>>,
}

impl MockLocalClockAccessor {
    /// Bump the amount of mock time that's passed.
    pub fn tick(&self, d: std::time::Duration) {
        let mut time = self.time.lock();
        *time = *time + LocalClockDelta::from_millis(d.as_millis().try_into().unwrap())
    }

    /// Bump the amount of mock time that's passed backwards in time.
    pub fn tick_backwards(&self, d: std::time::Duration) {
        let mut time = self.time.lock();
        *time = *time - LocalClockDelta::from_millis(d.as_millis().try_into().unwrap())
    }

    /// Get the current clock time.
    pub fn get_time(&self) -> LocalClockTime {
        *self.time.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn naive_system_time() {
        let mut clock = SystemTimeClock::new(LocalClockDelta::ZERO);

        let time = clock.get_time();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let new_time = clock.get_time();

        let delta = new_time - time;

        // cannot use assert_eq, because there is a *bit* of extra time elapsed
        // aside from the thread sleep.
        assert!(delta >= std::time::Duration::from_secs(1).into());
        assert!(delta < std::time::Duration::from_secs(2).into()); // sanity check
    }

    #[test]
    fn initial_delta() {
        let mut clock = SystemTimeClock::new(LocalClockDelta::from_millis(1000));

        let time = clock.get_time();
        let now = LocalClockTime::from(SystemTime::now());

        let delta = time - now;

        // cannot use assert_eq, because there is a *bit* of extra time elapsed
        // aside from the thread sleep.
        eprintln!("delta: {delta:?}");
        assert!(delta >= LocalClockDelta::from_millis(999)); // allow for rounding to lose a milli
        assert!(delta < LocalClockDelta::from_millis(2000)); // sanity check
    }

    #[test]
    fn naive_set_time_backwards() {
        let mut clock = SystemTimeClock::new(LocalClockDelta::ZERO);

        clock.set_time(LocalClockTime::from_millis_since_unix_epoch(0));

        let time = clock.get_time();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let new_time = clock.get_time();

        let delta = new_time - time;

        // cannot use assert_eq, because there is a *bit* of extra time elapsed
        // aside from the thread sleep.
        assert!(delta >= std::time::Duration::from_secs(1).into());
        assert!(delta < std::time::Duration::from_secs(2).into()); // sanity check
    }
}
