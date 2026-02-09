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
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Instant;
#[doc(hidden)]
pub use tracing;

const PERIOD_MS: u32 = 5000;
const EVENTS_PER_PERIOD: u32 = 10;

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
    state: Mutex<RateLimiterState>,
}

struct RateLimiterState {
    period_ms: u32,
    events_per_period: u32,
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
            state: Mutex::new(RateLimiterState {
                period_ms,
                events_per_period,
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
        self.event_with_config(None, None)
    }

    /// Returns `Ok(missed_events)` if this event should be logged.
    /// Optionally reconfigures the rate limiter if new parameters are provided.
    ///
    /// `missed_events` is `Some(n)` if there were any missed events or if this
    /// event is the last one before rate limiting kicks in.
    #[cold]
    pub fn event_with_config(
        &self,
        period_ms: Option<u32>,
        events_per_period: Option<u32>,
    ) -> Result<Option<u64>, RateLimited> {
        if DISABLE_RATE_LIMITING.load(Ordering::Relaxed) {
            return Ok(None);
        }

        let mut state = self.state.try_lock().ok_or(RateLimited)?;

        // Reconfigure if new parameters are provided
        let mut reset_state = false;
        if let Some(new_period) = period_ms {
            if state.period_ms != new_period {
                state.period_ms = new_period;
                reset_state = true;
            }
        }
        if let Some(new_events_per_period) = events_per_period {
            if state.events_per_period != new_events_per_period {
                state.events_per_period = new_events_per_period;
                reset_state = true;
            }
        }

        // Reset state when parameters change
        if reset_state {
            state.start = None;
            state.events = 0;
            state.missed = 0;
        }

        let now = Instant::now();
        let period_ms = state.period_ms;
        let start = state.start.get_or_insert(now);
        let elapsed = now.duration_since(*start);
        if elapsed.as_millis() > period_ms as u128 {
            *start = now;
            state.events = 0;
        }
        if state.events >= state.events_per_period {
            state.missed += 1;
            return Err(RateLimited);
        }
        state.events += 1;
        let missed = std::mem::take(&mut state.missed);
        let missed = (missed != 0 || state.events == state.events_per_period).then_some(missed);
        Ok(missed)
    }
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
            static RATE_LIMITER: $crate::RateLimiter = $crate::RateLimiter::new_default();
            if let Ok(missed_events) = RATE_LIMITER.event_with_config(Some($period), Some($limit)) {
                $crate::tracing::error!(dropped_ratelimited = missed_events, $($rest)*);
            }
        }
    };
    // With period only
    (period: $period:expr, $($rest:tt)*) => {
        {
            static RATE_LIMITER: $crate::RateLimiter = $crate::RateLimiter::new_default();
            if let Ok(missed_events) = RATE_LIMITER.event_with_config(Some($period), None) {
                $crate::tracing::error!(dropped_ratelimited = missed_events, $($rest)*);
            }
        }
    };
    // With limit only
    (limit: $limit:expr, $($rest:tt)*) => {
        {
            static RATE_LIMITER: $crate::RateLimiter = $crate::RateLimiter::new_default();
            if let Ok(missed_events) = RATE_LIMITER.event_with_config(None, Some($limit)) {
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
            static RATE_LIMITER: $crate::RateLimiter = $crate::RateLimiter::new_default();
            if let Ok(missed_events) = RATE_LIMITER.event_with_config(Some($period), Some($limit)) {
                $crate::tracing::warn!(dropped_ratelimited = missed_events, $($rest)*);
            }
        }
    };
    // With period only
    (period: $period:expr, $($rest:tt)*) => {
        {
            static RATE_LIMITER: $crate::RateLimiter = $crate::RateLimiter::new_default();
            if let Ok(missed_events) = RATE_LIMITER.event_with_config(Some($period), None) {
                $crate::tracing::warn!(dropped_ratelimited = missed_events, $($rest)*);
            }
        }
    };
    // With limit only
    (limit: $limit:expr, $($rest:tt)*) => {
        {
            static RATE_LIMITER: $crate::RateLimiter = $crate::RateLimiter::new_default();
            if let Ok(missed_events) = RATE_LIMITER.event_with_config(None, Some($limit)) {
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
            static RATE_LIMITER: $crate::RateLimiter = $crate::RateLimiter::new_default();
            if let Ok(missed_events) = RATE_LIMITER.event_with_config(Some($period), Some($limit)) {
                $crate::tracing::info!(dropped_ratelimited = missed_events, $($rest)*);
            }
        }
    };
    // With period only
    (period: $period:expr, $($rest:tt)*) => {
        {
            static RATE_LIMITER: $crate::RateLimiter = $crate::RateLimiter::new_default();
            if let Ok(missed_events) = RATE_LIMITER.event_with_config(Some($period), None) {
                $crate::tracing::info!(dropped_ratelimited = missed_events, $($rest)*);
            }
        }
    };
    // With limit only
    (limit: $limit:expr, $($rest:tt)*) => {
        {
            static RATE_LIMITER: $crate::RateLimiter = $crate::RateLimiter::new_default();
            if let Ok(missed_events) = RATE_LIMITER.event_with_config(None, Some($limit)) {
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

/// As [`tracing::event!`], but rate limited.
///
/// Can be called with optional parameters to customize rate limiting:
/// - `period: <ms>` - rate limiting period in milliseconds
/// - `limit: <count>` - maximum events per period
///
/// `level` is required and must be a compile-time literal identifier (ERROR, WARN, INFO, DEBUG, TRACE).
#[macro_export]
macro_rules! event_ratelimited_static {
    // With both period and limit and level
    (level: $level:ident, period: $period:expr, limit: $limit:expr, $($rest:tt)*) => {{
        static RATE_LIMITER: $crate::RateLimiter = $crate::RateLimiter::new_default();
        if let Ok(missed_events) = RATE_LIMITER.event_with_config(Some($period), Some($limit)) {
            $crate::tracing::event!(
                $crate::tracing::Level::$level,
                dropped_ratelimited = missed_events,
                $($rest)*
            );
        }
    }};
    // With only period and level
    (level: $level:ident, period: $period:expr, $($rest:tt)*) => {{
        static RATE_LIMITER: $crate::RateLimiter = $crate::RateLimiter::new_default();
        if let Ok(missed_events) = RATE_LIMITER.event_with_config(Some($period), None) {
            $crate::tracing::event!(
                $crate::tracing::Level::$level,
                dropped_ratelimited = missed_events,
                $($rest)*
            );
        }
    }};
    // With only limit and level
    (level: $level:ident, limit: $limit:expr, $($rest:tt)*) => {{
        static RATE_LIMITER: $crate::RateLimiter = $crate::RateLimiter::new_default();
        if let Ok(missed_events) = RATE_LIMITER.event_with_config(None, Some($limit)) {
            $crate::tracing::event!(
                $crate::tracing::Level::$level,
                dropped_ratelimited = missed_events,
                $($rest)*
            );
        }
    }};
    // Default case (only level provided)
    (level: $level:ident, $($rest:tt)*) => {{
        static RATE_LIMITER: $crate::RateLimiter = $crate::RateLimiter::new_default();
        if let Ok(missed_events) = RATE_LIMITER.event() {
            $crate::tracing::event!(
                $crate::tracing::Level::$level,
                dropped_ratelimited = missed_events,
                $($rest)*
            );
        }
    }};
}

/// Helper macro for dynamically dispatching to [`event_ratelimited_static!`] based on a runtime level.
///
/// This macro accepts a runtime `tracing::Level` expression and dispatches to the appropriate
/// compile-time level identifier. Allows the log level to be determined at runtime.
///
/// Examples:
/// ```
/// use tracing::Level;
/// use tracelimit::event_ratelimited;
/// event_ratelimited!(Level::ERROR, period: 1000, limit: 5, "custom period and limit");
/// event_ratelimited!(Level::WARN, period: 10000, "custom period only");
/// event_ratelimited!(Level::INFO, limit: 50, "custom limit only");
/// event_ratelimited!(Level::TRACE, "simple message");
/// ```
#[macro_export]
macro_rules! event_ratelimited {
    // With period and limit and level
    ($level:expr, period: $period:expr, limit: $limit:expr, $($rest:tt)*) => {
        match $level {
            $crate::tracing::Level::ERROR => {
                $crate::event_ratelimited_static!(level: ERROR, period: $period, limit: $limit, $($rest)*);
            }
            $crate::tracing::Level::WARN => {
                $crate::event_ratelimited_static!(level: WARN, period: $period, limit: $limit, $($rest)*);
            }
            $crate::tracing::Level::INFO => {
                $crate::event_ratelimited_static!(level: INFO, period: $period, limit: $limit, $($rest)*);
            }
            $crate::tracing::Level::DEBUG => {
                $crate::event_ratelimited_static!(level: DEBUG, period: $period, limit: $limit, $($rest)*);
            }
            $crate::tracing::Level::TRACE => {
                $crate::event_ratelimited_static!(level: TRACE, period: $period, limit: $limit, $($rest)*);
            }
        }
    };
    // With period and level
    ($level:expr, period: $period:expr, $($rest:tt)*) => {
        match $level {
            $crate::tracing::Level::ERROR => {
                $crate::event_ratelimited_static!(level: ERROR, period: $period, $($rest)*);
            }
            $crate::tracing::Level::WARN => {
                $crate::event_ratelimited_static!(level: WARN, period: $period, $($rest)*);
            }
            $crate::tracing::Level::INFO => {
                $crate::event_ratelimited_static!(level: INFO, period: $period, $($rest)*);
            }
            $crate::tracing::Level::DEBUG => {
                $crate::event_ratelimited_static!(level: DEBUG, period: $period, $($rest)*);
            }
            $crate::tracing::Level::TRACE => {
                $crate::event_ratelimited_static!(level: TRACE, period: $period, $($rest)*);
            }
        }
    };
    // With limit and level
    ($level:expr, limit: $limit:expr, $($rest:tt)*) => {
        match $level {
            $crate::tracing::Level::ERROR => {
                $crate::event_ratelimited_static!(level: ERROR, limit: $limit, $($rest)*);
            }
            $crate::tracing::Level::WARN => {
                $crate::event_ratelimited_static!(level: WARN, limit: $limit, $($rest)*);
            }
            $crate::tracing::Level::INFO => {
                $crate::event_ratelimited_static!(level: INFO, limit: $limit, $($rest)*);
            }
            $crate::tracing::Level::DEBUG => {
                $crate::event_ratelimited_static!(level: DEBUG, limit: $limit, $($rest)*);
            }
            $crate::tracing::Level::TRACE => {
                $crate::event_ratelimited_static!(level: TRACE, limit: $limit, $($rest)*);
            }
        }
    };
    // Default case (only level provided)
    ($level:expr, $($rest:tt)*) => {
        match $level {
            $crate::tracing::Level::ERROR => {
                $crate::event_ratelimited_static!(level: ERROR, $($rest)*);
            }
            $crate::tracing::Level::WARN => {
                $crate::event_ratelimited_static!(level: WARN, $($rest)*);
            }
            $crate::tracing::Level::INFO => {
                $crate::event_ratelimited_static!(level: INFO, $($rest)*);
            }
            $crate::tracing::Level::DEBUG => {
                $crate::event_ratelimited_static!(level: DEBUG, $($rest)*);
            }
            $crate::tracing::Level::TRACE => {
                $crate::event_ratelimited_static!(level: TRACE, $($rest)*);
            }
        }
    };
}
