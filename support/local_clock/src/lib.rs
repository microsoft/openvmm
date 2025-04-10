// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! An instance-local, reprogrammable, real time clock.
//!
//! This crate's primary export is the [`LocalClock`] trait, which defines an
//! interface for interacting with instances of reprogrammable real time clocks,
//! decoupled from any particular platform's global real time clock.
//!
//! Also included in `local_time` is [`SystemTimeClock`], which is a basic
//! in-tree implementation of [`LocalClock`] backed by
//! [`std::time::SystemTime`]. Unless you're worried about handling instances
//! where `SystemTime` jumps backwards, this is perfectly reasonable
//! implementation to use.
//!
//! This crate deliberately takes a minimalist approach to dependencies, and
//! avoids pulling in any "heavyweight" date/time crates by default (e.g:
//! `time`, `chrono`, etc...). That said, native integration with some of these
//! crates is provided via optional features.
//!
//! # Features
//!
//! - `inspect` - Derives the [`Inspect`] trait on various types.
//! - `time_exts` - Integration with the [`time`] crate
//!     - Provides `From`/`Into` implementations to interop with duration and
//!       date/time types from `time`.
//!     - Changes the `Debug` implementation of [`LocalClockTime`] to print a
//!       human readable date (instead of a raw duration since the Unix Epoch).
//!
//! [`Inspect`]: inspect::Inspect

mod clock_impls;

pub use clock_impls::MockLocalClock;
pub use clock_impls::MockLocalClockAccessor;
pub use clock_impls::SystemTimeClock;
#[cfg(feature = "inspect")]
pub use inspect_ext::InspectableLocalClock;

use std::fmt::Debug;

/// A local real-time clock, hanging-off the platform's global real-time clock.
///
/// One way to think about [`LocalClock`] is that it matches the semantics of
/// the POSIX methods `clock_gettime` and `clock_settime` when backed by
/// `CLOCK_REALTIME`, except setting the time on a [`LocalClock`] will only
/// affect that particular instance (as opposed to changing the global system
/// time).
///
/// NOTE: These methods may be invoked fairly often, and as such, implementors
/// should ensure these methods do not block!
pub trait LocalClock: Send {
    /// Return the current clock time.
    ///
    /// # First call
    ///
    /// If `set_time` has yet to be called, the `LocalClock` trait makes _no
    /// guarantees_ as to what time `get_time` will return!
    ///
    /// Conceptually, this would be akin to pulling the real-time-block battery
    /// from a physical machine, thereby resetting the clock to its "default"
    /// state - whatever that might be.
    ///
    /// A simple implementation would be to just return a hard-coded, fixed
    /// value, corresponding to some arbitrary date.
    ///
    /// ...that being said, a far more useful implementation would be to simply
    /// report the platform's current real time. That way, even if `set_time`
    /// never gets invoked, (e.g: as a result of communicating with a NTP
    /// server, a synthetic real-time assist virtual device, manual user input,
    /// etc...), the reported real time will still be reasonably close to the
    /// current date.
    ///
    /// # Subsequent calls
    ///
    /// On subsequent calls to this function, this method MUST return the sum of
    /// the previously set time set via `set_time`, _plus_ the wall-clock time
    /// that has elapsed since.
    ///
    /// NOTE: implementations SHOULD ensure that real time continues to tick
    /// even when the device / platform itself has shutdown / been paused.
    fn get_time(&mut self) -> LocalClockTime;

    /// Set the current clock time.
    fn set_time(&mut self, new_time: LocalClockTime);
}

/// A delta between two [`LocalClockTime`] instances.
///
/// Unlike [`std::time::Duration`], a `LocalClockDelta` may be negative, as
/// unlike [`std::time::Instant`] or [`std::time::SystemTime`], it's perfectly
/// reasonable (and expected!) that [`LocalClock`] returns a [`LocalClockTime`]
/// that is _earlier_ than a previously returned `LocalClockTime` (as would be
/// the case if a `LocalClock` is re-programmed to an earlier time).
///
/// This type doesn't expose a particularly "rich" API for working the the
/// contained time delta. Rather, consumers of this type are expected to us it
/// alongside an external time/date library (such as `time` or `chrono`) in
/// order to more easily manipulate the time delta.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
#[cfg_attr(feature = "inspect", inspect(debug))]
pub struct LocalClockDelta {
    // DEVNOTE: see DEVNOTE in `LocalClockTime` for rationale around storing
    // duration in units of milliseconds.
    millis: i64,
}

impl Debug for LocalClockDelta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}{:?}",
            if self.millis.is_negative() { "-" } else { "" },
            std::time::Duration::from_millis(self.millis.unsigned_abs())
        )
    }
}

impl LocalClockDelta {
    /// A delta of zero milliseconds.
    pub const ZERO: Self = Self { millis: 0 };

    /// Return the duration in milliseconds.
    pub const fn as_millis(self) -> i64 {
        self.millis
    }

    /// Create a duration from milliseconds.
    pub const fn from_millis(millis: i64) -> Self {
        Self { millis }
    }
}

/// An opaque type, representing an instant in time.
///
/// This type doesn't expose a particularly "rich" API for working the the
/// contained time. Rather, consumers of this type are expected to us it
/// alongside an external time/date library (such as `time` or `chrono`) in
/// order to more easily manipulate time/date values.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
#[cfg_attr(feature = "inspect", inspect(debug))]
pub struct LocalClockTime {
    // A couple DEVNOTES:
    //
    // - The decision to store time using the Unix Epoch as an "anchor" was
    //   primarily chosen out of convenience, and could just as well be swapped
    //   out for something else if that turns out to be more expedient.
    //
    // - The decision to store time in units of milliseconds was a purely
    //   pragmatic one: RTC devices typically don't have resolutions greater
    //   than 1ms. That said, if this assumption turns out to be untrue, the
    //   fact that `LocalClockTime` is an opaque type means that we could
    //   non-intrusively bump the contained resolution without affecting
    //   existing consumers.
    //
    // - i64::MAX / i64::MIN correspond to ~300000000 years on either end of the
    //   unix epoch, in case you're worried about not being able to represent a
    //   particular date.
    millis_since_epoch: i64,
}

impl Debug for LocalClockTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        #[cfg(not(feature = "time_exts"))]
        {
            write!(
                f,
                "{}{:?}",
                if self.millis_since_epoch.is_negative() {
                    "-"
                } else {
                    ""
                },
                std::time::Duration::from_millis(self.millis_since_epoch.unsigned_abs())
            )
        }

        #[cfg(feature = "time_exts")]
        {
            let date_time: Result<time::OffsetDateTime, _> = (*self).try_into();
            match date_time {
                Ok(date_time) => write!(f, "{}", date_time),
                Err(e) => write!(f, "{:?}", e),
            }
        }
    }
}

impl LocalClockTime {
    /// Return the number of millis since the Unix Epoch (in UTC).
    ///
    /// Also see: [`std::time::UNIX_EPOCH`]
    pub fn as_millis_since_unix_epoch(&self) -> i64 {
        self.millis_since_epoch
    }

    /// Create a new [`LocalClockTime`] containing the time some number of
    /// millis offset from the Unix Epoch (in UTC).
    ///
    /// Also see: [`std::time::UNIX_EPOCH`]
    pub fn from_millis_since_unix_epoch(millis: i64) -> Self {
        Self {
            millis_since_epoch: millis,
        }
    }
}

impl std::ops::Sub for LocalClockDelta {
    type Output = LocalClockDelta;

    fn sub(self, rhs: Self) -> Self::Output {
        LocalClockDelta {
            millis: self.millis - rhs.millis,
        }
    }
}

impl std::ops::Add for LocalClockDelta {
    type Output = LocalClockDelta;

    fn add(self, rhs: Self) -> Self::Output {
        LocalClockDelta {
            millis: self.millis + rhs.millis,
        }
    }
}

impl std::ops::Sub for LocalClockTime {
    type Output = LocalClockDelta;

    fn sub(self, rhs: Self) -> Self::Output {
        LocalClockDelta::from_millis(self.millis_since_epoch - rhs.millis_since_epoch)
    }
}

impl std::ops::Sub<LocalClockDelta> for LocalClockTime {
    type Output = LocalClockTime;

    fn sub(self, rhs: LocalClockDelta) -> Self::Output {
        LocalClockTime {
            millis_since_epoch: self.millis_since_epoch - rhs.as_millis(),
        }
    }
}

impl std::ops::Add<LocalClockDelta> for LocalClockTime {
    type Output = LocalClockTime;

    fn add(self, rhs: LocalClockDelta) -> Self::Output {
        LocalClockTime {
            millis_since_epoch: self.millis_since_epoch + rhs.as_millis(),
        }
    }
}

// only allow one-way conversion, as std::time::Duration doesn't support
// negative time
impl From<std::time::Duration> for LocalClockDelta {
    fn from(d: std::time::Duration) -> Self {
        LocalClockDelta::from_millis(d.as_millis().try_into().unwrap())
    }
}

impl From<std::time::SystemTime> for LocalClockTime {
    fn from(sys_time: std::time::SystemTime) -> Self {
        let millis_since_epoch = if std::time::UNIX_EPOCH < sys_time {
            let since_epoch = sys_time.duration_since(std::time::UNIX_EPOCH).unwrap();
            let millis: i64 = since_epoch.as_millis().try_into().unwrap();
            millis
        } else {
            let since_epoch = std::time::UNIX_EPOCH.duration_since(sys_time).unwrap();
            let millis: i64 = since_epoch.as_millis().try_into().unwrap();
            -millis
        };

        LocalClockTime::from_millis_since_unix_epoch(millis_since_epoch)
    }
}

impl From<LocalClockTime> for std::time::SystemTime {
    fn from(clock_time: LocalClockTime) -> Self {
        let millis_since_epoch = clock_time.as_millis_since_unix_epoch();
        if millis_since_epoch.is_negative() {
            let before_epoch = std::time::Duration::from_millis((-millis_since_epoch) as u64);
            std::time::UNIX_EPOCH.checked_sub(before_epoch).unwrap()
        } else {
            let after_epoch = std::time::Duration::from_millis(millis_since_epoch as u64);
            std::time::UNIX_EPOCH.checked_add(after_epoch).unwrap()
        }
    }
}

/// Indicates that an overflow error occurred during conversion.
#[derive(Debug)]
pub struct OverflowError;

#[cfg(feature = "time_exts")]
mod time_ext {
    use super::LocalClockDelta;
    use super::LocalClockTime;

    impl From<time::OffsetDateTime> for LocalClockTime {
        fn from(date_time: time::OffsetDateTime) -> LocalClockTime {
            let since_epoch = date_time - time::OffsetDateTime::UNIX_EPOCH;
            LocalClockTime::from_millis_since_unix_epoch(
                since_epoch.whole_milliseconds().try_into().unwrap(),
            )
        }
    }

    impl From<time::Duration> for LocalClockDelta {
        fn from(time_duration: time::Duration) -> LocalClockDelta {
            LocalClockDelta::from_millis(time_duration.whole_milliseconds().try_into().unwrap())
        }
    }

    impl From<LocalClockDelta> for time::Duration {
        fn from(clock_duration: LocalClockDelta) -> time::Duration {
            time::Duration::milliseconds(clock_duration.as_millis())
        }
    }

    impl TryFrom<LocalClockTime> for time::OffsetDateTime {
        type Error = super::OverflowError;

        fn try_from(clock_time: LocalClockTime) -> Result<time::OffsetDateTime, Self::Error> {
            let duration = time::Duration::milliseconds(clock_time.as_millis_since_unix_epoch());
            time::OffsetDateTime::UNIX_EPOCH
                .checked_add(duration)
                .ok_or(super::OverflowError)
        }
    }
}

/// Defines a trait that combines LocalClock and Inspect
#[cfg(feature = "inspect")]
mod inspect_ext {
    use super::*;
    use inspect::Inspect;

    /// Extends [`LocalClock`] with a bound on [`Inspect`]
    pub trait InspectableLocalClock: LocalClock + Inspect {}
    impl<T: LocalClock + Inspect> InspectableLocalClock for T {}
}
