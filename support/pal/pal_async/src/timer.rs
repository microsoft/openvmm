// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Timer-related functionality.

use crate::driver::Driver;
use crate::driver::PollImpl;
use crate::sparsevec::SparseVec;
use crate::waker::WakerList;
use std::future::Future;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;

/// Instant represents a number of nanoseconds since some process-specific
/// epoch.
///
/// On Windows this is backed by QueryUnbiasedInterruptTimePrecise. On
/// Linux this is backed by CLOCK_MONOTONIC.
///
/// This is modeled after std::time::Instant but uses a different clock source
/// on Windows, and it allows access to the raw value.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct Instant(u64);

impl Instant {
    /// The current time as measured by this clock.
    pub fn now() -> Self {
        Self(crate::sys::monotonic_nanos_now())
    }

    /// Returns the value of the underlying clock in nanosecond resolution.
    pub fn as_nanos(&self) -> u64 {
        self.0
    }

    /// Creates an instant from an underlying clock value in nanosecond resolution.
    pub fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    /// Adds a duration to this instant, saturating at the maximum value of the
    /// clock.
    pub fn saturating_add(self, duration: Duration) -> Self {
        Self(
            self.0
                .saturating_add(duration.as_nanos().try_into().unwrap_or(u64::MAX)),
        )
    }

    /// Calculate the duration between this instant and another instant,
    /// saturating at zero if the other instant is later than this one.
    pub fn saturating_sub(self, rhs: Instant) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(rhs.0))
    }
}

impl std::ops::Sub for Instant {
    type Output = Duration;
    fn sub(self, rhs: Instant) -> Self::Output {
        Duration::from_nanos(
            self.0.checked_sub(rhs.0).unwrap_or_else(|| {
                panic!("supplied instant {:#x} is later than {:#x}", rhs.0, self.0)
            }),
        )
    }
}

impl std::ops::Add<Duration> for Instant {
    type Output = Instant;
    fn add(self, rhs: Duration) -> Self::Output {
        Self(
            self.0
                .checked_add(rhs.as_nanos().try_into().expect("duration too large"))
                .expect("supplied duration causes overflow"),
        )
    }
}

impl std::ops::Sub<Duration> for Instant {
    type Output = Instant;
    fn sub(self, rhs: Duration) -> Self::Output {
        Self(
            self.0
                .checked_sub(rhs.as_nanos().try_into().expect("duration too large"))
                .expect("supplied instant is later than self"),
        )
    }
}

/// A trait for driving timers.
pub trait TimerDriver: Unpin {
    /// The timer type.
    type Timer: 'static + PollTimer;

    /// Returns a new timer.
    fn new_timer(&self) -> Self::Timer;
}

/// A trait for polling timers.
pub trait PollTimer: Unpin + Send + Sync {
    /// Polls the timer, optionally updating the deadline first.
    ///
    /// Returns ready with the current time when the set deadline <=
    /// `Instant::now()`.
    fn poll_timer(&mut self, cx: &mut Context<'_>, deadline: Option<Instant>) -> Poll<Instant>;

    /// Updates the timer's deadline.
    fn set_deadline(&mut self, deadline: Instant);
}

/// An asynchronous timer.
pub struct PolledTimer(PollImpl<dyn PollTimer>);

impl PolledTimer {
    /// Creates a new timer.
    pub fn new(driver: &(impl ?Sized + Driver)) -> Self {
        Self(driver.new_dyn_timer())
    }

    /// Delays the current task for `duration`.
    pub fn sleep(&mut self, duration: Duration) -> Sleep<'_> {
        self.sleep_until(Instant::now() + duration)
    }

    /// Delays the current task until `deadline`.
    pub fn sleep_until(&mut self, deadline: Instant) -> Sleep<'_> {
        self.0.set_deadline(deadline);
        Sleep {
            timer: self,
            deadline,
        }
    }

    /// Returns `Pending` until the current time is later than `deadline`. Then
    /// returns `Ready` with the current time.
    pub fn poll_until(&mut self, cx: &mut Context<'_>, deadline: Instant) -> Poll<Instant> {
        self.0.poll_timer(cx, Some(deadline))
    }
}

/// [`Future`] implementation for [`PolledTimer::sleep`] and
/// [`PolledTimer::sleep_until`].
#[must_use]
pub struct Sleep<'a> {
    timer: &'a mut PolledTimer,
    deadline: Instant,
}

impl Future for Sleep<'_> {
    type Output = Instant;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let deadline = self.deadline;
        self.timer.0.poll_timer(cx, Some(deadline))
    }
}

#[derive(Debug)]
struct TimerEntry {
    deadline: Instant,
    waker: Option<Waker>,
}

/// A queue of timers.
///
/// Used to multiplex multiple timers on a single timeout operation.
#[derive(Debug, Default)]
pub(crate) struct TimerQueue {
    timers: SparseVec<TimerEntry>,
}

/// A timer ID.
#[derive(Debug, Copy, Clone)]
pub(crate) struct TimerQueueId(usize);

pub(crate) enum TimerResult {
    TimedOut(Instant),
    Pending(Instant),
}

impl TimerQueue {
    /// Adds a new timer.
    pub fn add(&mut self) -> TimerQueueId {
        TimerQueueId(self.timers.add(TimerEntry {
            deadline: Instant::from_nanos(0),
            waker: None,
        }))
    }

    /// Removes a timer.
    ///
    /// Don't wake the returned waker, just ensure it's not dropped while
    /// holding a lock.
    #[must_use]
    pub fn remove(&mut self, id: TimerQueueId) -> Option<Waker> {
        self.timers.remove(id.0).waker
    }

    /// Polls a timer for completion.
    pub fn poll_deadline(&mut self, cx: &mut Context<'_>, id: TimerQueueId) -> TimerResult {
        let timer = &mut self.timers[id.0];
        let now = Instant::now();
        if timer.deadline <= now {
            TimerResult::TimedOut(now)
        } else {
            let waker = cx.waker();
            if let Some(old_waker) = &mut timer.waker {
                old_waker.clone_from(waker);
            } else {
                timer.waker = Some(waker.clone());
            }
            TimerResult::Pending(timer.deadline)
        }
    }

    /// Sets the deadline for a timer.
    ///
    /// Returns true if the backing timer may need to be adjusted.
    pub fn set_deadline(&mut self, id: TimerQueueId, deadline: Instant) -> bool {
        let timer = &mut self.timers[id.0];
        let update = timer.waker.is_some() && timer.deadline > deadline;
        timer.deadline = deadline;
        update
    }

    /// Returns wakers for any expired timers.
    pub fn wake_expired(&mut self, wakers: &mut WakerList) {
        let mut now = None;
        wakers.extend(self.timers.iter_mut().filter_map(|(_, timer)| {
            if timer.waker.is_some() && timer.deadline <= *now.get_or_insert_with(Instant::now) {
                let waker = timer.waker.take().unwrap();
                Some(waker)
            } else {
                None
            }
        }))
    }

    /// Returns the deadline of the next timer, or `None` if there are no unexpired timers.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.timers
            .iter()
            .filter_map(|(_, entry)| entry.waker.is_some().then_some(entry.deadline))
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::Instant;
    use std::time::Duration;

    #[test]
    fn test_instant() {
        let start = Instant::now();
        std::thread::sleep(Duration::from_millis(100));
        let end = Instant::now();
        assert!(end - start >= Duration::from_millis(100));
        assert!(end - start < Duration::from_millis(400));
    }
}
