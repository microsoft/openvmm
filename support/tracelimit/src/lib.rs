// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementations of [`tracing`] macros that are rate limited.
//!
//! These are useful when an untrusted entity can arbitrarily trigger an event
//! to be logged. In that case, rate limiting the events can prevent other
//! important events from being lost.
//!
//! Note that there currently are no macros for rate limiting debug or trace
//! level events. This is due to an implementation limitation--the macros in
//! this crate check the rate limit before evaluating whether the event should
//! be logged, and so this would add an extra check to every debug/trace event.
//! This could be fixed by using some of the hidden but `pub` machinery of the
//! `tracing` crate, which is probably not worth it for now.

#![forbid(unsafe_code)]

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Instant;
#[doc(hidden)]
pub use tracing;

#[doc(hidden)]
pub const PERIOD_MS: u32 = 5000;
#[doc(hidden)]
pub const EVENTS_PER_PERIOD: u32 = 10;

static DISABLE_RATE_LIMITING: AtomicBool = AtomicBool::new(false);

/// Disables or reenables rate limiting globally.
///
/// Rate limiting defaults to enabled. You might want to disable it during local
/// development or in tests.
pub fn disable_rate_limiting(disabled: bool) {
    DISABLE_RATE_LIMITING.store(disabled, Ordering::Relaxed);
}

#[doc(hidden)]
pub struct RateLimiter {
    period_ms: u32,
    events_per_period: u32,
    state: Mutex<RateLimiterState>,
}

struct RateLimiterState {
    start: Option<Instant>,
    events: u32,
    missed: u64,
}

#[doc(hidden)]
pub struct RateLimited;

impl RateLimiter {
    pub const fn new_default() -> Self {
        Self::new(PERIOD_MS, EVENTS_PER_PERIOD)
    }

    pub const fn new(period_ms: u32, events_per_period: u32) -> Self {
        Self {
            period_ms,
            events_per_period,
            state: Mutex::new(RateLimiterState {
                start: None,
                events: 0,
                missed: 0,
            }),
        }
    }

    /// Returns `Ok(missed_events)` if this event should be logged.
    ///
    /// `missed_events` is `Some(n)` if there were any missed events or if this
    /// event is the last one before rate limiting kicks in.
    pub fn event(&self) -> Result<Option<u64>, RateLimited> {
        if DISABLE_RATE_LIMITING.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let mut state = self.state.try_lock().ok_or(RateLimited)?;
        let now = Instant::now();
        let start = state.start.get_or_insert(now);
        let elapsed = now.duration_since(*start);
        if elapsed.as_millis() > self.period_ms as u128 {
            *start = now;
            state.events = 0;
        }
        if state.events >= self.events_per_period {
            state.missed += 1;
            return Err(RateLimited);
        }
        state.events += 1;
        let missed = std::mem::take(&mut state.missed);
        let missed = (missed != 0 || state.events == self.events_per_period).then_some(missed);
        Ok(missed)
    }
}

// Runtime rate limiter support
//
// NOTE: We can't use static rate limiters when users pass runtime variables to macros
// The solution is to use a global cache of rate limiters created on-demand
//
// HashMap: Cache rate limiters to preserve state across calls
// Mutex: Thread-safe access to the HashMap
// LazyLock: Initialize HashMap on first use
//
// The keys are (period, limit, call_site) to ensure that each macro location gets
// independent rate limiting.
static RUNTIME_RATE_LIMITERS: LazyLock<Mutex<HashMap<(u32, u32, &'static str), RateLimiter>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Get or create a runtime rate limiter for the given parameters and call site.
/// This will reuse existing rate limiters if they exist, otherwise create a new one.
#[doc(hidden)]
pub fn get_runtime_rate_limiter(
    period_ms: u32,
    events_per_period: u32,
    call_site: &'static str,
) -> Result<Option<u64>, RateLimited> {
    if DISABLE_RATE_LIMITING.load(Ordering::Relaxed) {
        return Ok(None);
    }

    let key = (period_ms, events_per_period, call_site);
    let mut limiters = RUNTIME_RATE_LIMITERS.lock();

    let limiter = limiters
        .entry(key)
        .or_insert_with(|| RateLimiter::new(period_ms, events_per_period));

    limiter.event()
}

/// As [`tracing::error!`], but rate limited.
///
/// Can be called with optional parameters to customize rate limiting:
/// - `period: <ms>` - rate limiting period in milliseconds
/// - `limit: <count>` - maximum events per period
///
/// Examples:
/// ```
/// use tracelimit::error_ratelimited;
/// error_ratelimited!("simple message");
/// error_ratelimited!(period: 1000, limit: 5, "custom rate limit");
/// error_ratelimited!(period: 10000, "custom period only");
/// error_ratelimited!(limit: 50, "custom limit only");
/// ```
#[macro_export]
macro_rules! error_ratelimited {
    // With both period and limit
    (period: $period:expr, limit: $limit:expr, $($rest:tt)*) => {
        {
            if let Ok(missed_events) = $crate::get_runtime_rate_limiter(
                $period,
                $limit,
                concat!(file!(), ":", line!(), ":", column!())
            ) {
                $crate::tracing::error!(dropped_ratelimited = missed_events, $($rest)*);
            }
        }
    };
    // With period only
    (period: $period:expr, $($rest:tt)*) => {
        {
            if let Ok(missed_events) = $crate::get_runtime_rate_limiter(
                $period,
                $crate::EVENTS_PER_PERIOD,
                concat!(file!(), ":", line!(), ":", column!())
            ) {
                $crate::tracing::error!(dropped_ratelimited = missed_events, $($rest)*);
            }
        }
    };
    // With limit only
    (limit: $limit:expr, $($rest:tt)*) => {
        {
            if let Ok(missed_events) = $crate::get_runtime_rate_limiter(
                $crate::PERIOD_MS,
                $limit,
                concat!(file!(), ":", line!(), ":", column!())
            ) {
                $crate::tracing::error!(dropped_ratelimited = missed_events, $($rest)*);
            }
        }
    };
    // Default case (no custom parameters)
    ($($rest:tt)*) => {
        {
            static RATE_LIMITER: $crate::RateLimiter = $crate::RateLimiter::new_default();
            if let Ok(missed_events) = RATE_LIMITER.event() {
                $crate::tracing::error!(dropped_ratelimited = missed_events, $($rest)*);
            }
        }
    };
}

/// As [`tracing::warn!`], but rate limited.
///
/// Can be called with optional parameters to customize rate limiting:
/// - `period: <ms>` - rate limiting period in milliseconds
/// - `limit: <count>` - maximum events per period
///
/// Examples:
/// ```
/// use tracelimit::warn_ratelimited;
/// warn_ratelimited!("simple message");
/// warn_ratelimited!(period: 1000, limit: 5, "custom rate limit");
/// warn_ratelimited!(period: 10000, "custom period only");
/// warn_ratelimited!(limit: 50, "custom limit only");
/// ```
#[macro_export]
macro_rules! warn_ratelimited {
    // With both period and limit
    (period: $period:expr, limit: $limit:expr, $($rest:tt)*) => {
        {
            if let Ok(missed_events) = $crate::get_runtime_rate_limiter(
                $period,
                $limit,
                concat!(file!(), ":", line!(), ":", column!())
            ) {
                $crate::tracing::warn!(dropped_ratelimited = missed_events, $($rest)*);
            }
        }
    };
    // With period only
    (period: $period:expr, $($rest:tt)*) => {
        {
            if let Ok(missed_events) = $crate::get_runtime_rate_limiter(
                $period,
                $crate::EVENTS_PER_PERIOD,
                concat!(file!(), ":", line!(), ":", column!())
            ) {
                $crate::tracing::warn!(dropped_ratelimited = missed_events, $($rest)*);
            }
        }
    };
    // With limit only
    (limit: $limit:expr, $($rest:tt)*) => {
        {
            if let Ok(missed_events) = $crate::get_runtime_rate_limiter(
                $crate::PERIOD_MS,
                $limit,
                concat!(file!(), ":", line!(), ":", column!())
            ) {
                $crate::tracing::warn!(dropped_ratelimited = missed_events, $($rest)*);
            }
        }
    };
    // Default case (no custom parameters)
    ($($rest:tt)*) => {
        {
            static RATE_LIMITER: $crate::RateLimiter = $crate::RateLimiter::new_default();
            if let Ok(missed_events) = RATE_LIMITER.event() {
                $crate::tracing::warn!(dropped_ratelimited = missed_events, $($rest)*);
            }
        }
    };
}

/// As [`tracing::info!`], but rate limited.
///
/// Can be called with optional parameters to customize rate limiting:
/// - `period: <ms>` - rate limiting period in milliseconds
/// - `limit: <count>` - maximum events per period
///
/// Examples:
/// ```
/// use tracelimit::info_ratelimited;
/// info_ratelimited!("simple message");
/// info_ratelimited!(period: 1000, limit: 5, "custom rate limit");
/// info_ratelimited!(period: 10000, "custom period only");
/// info_ratelimited!(limit: 50, "custom limit only");
/// ```
#[macro_export]
macro_rules! info_ratelimited {
    // With both period and limit
    (period: $period:expr, limit: $limit:expr, $($rest:tt)*) => {
        {
            if let Ok(missed_events) = $crate::get_runtime_rate_limiter(
                $period,
                $limit,
                concat!(file!(), ":", line!(), ":", column!())
            ) {
                $crate::tracing::info!(dropped_ratelimited = missed_events, $($rest)*);
            }
        }
    };
    // With period only
    (period: $period:expr, $($rest:tt)*) => {
        {
            if let Ok(missed_events) = $crate::get_runtime_rate_limiter(
                $period,
                $crate::EVENTS_PER_PERIOD,
                concat!(file!(), ":", line!(), ":", column!())
            ) {
                $crate::tracing::info!(dropped_ratelimited = missed_events, $($rest)*);
            }
        }
    };
    // With limit only
    (limit: $limit:expr, $($rest:tt)*) => {
        {
            if let Ok(missed_events) = $crate::get_runtime_rate_limiter(
                $crate::PERIOD_MS,
                $limit,
                concat!(file!(), ":", line!(), ":", column!())
            ) {
                $crate::tracing::info!(dropped_ratelimited = missed_events, $($rest)*);
            }
        }
    };
    // Default case (no custom parameters)
    ($($rest:tt)*) => {
        {
            static RATE_LIMITER: $crate::RateLimiter = $crate::RateLimiter::new_default();
            if let Ok(missed_events) = RATE_LIMITER.event() {
                $crate::tracing::info!(dropped_ratelimited = missed_events, $($rest)*);
            }
        }
    };
}
