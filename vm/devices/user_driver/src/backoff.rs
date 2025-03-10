// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Async-friendly spin loop support.

use pal_async::driver::Driver;
use pal_async::timer::PolledTimer;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

const YIELD_ATTEMPTS: u64 = 250;
const SHORT_SLEEP_ATTEMPTS: u64 = 250;
const SHORT_SLEEP_DURATION: Duration = Duration::from_millis(1);
const LONG_SLEEP_DURATION: Duration = Duration::from_millis(15);

/// An object to yield execution of an async task while in a spin loop.
///
/// This is useful when waiting for some condition (such as a device register to
/// change states) that can only be polled and does not have an interrupt-based
/// notification.
pub struct Backoff<'a> {
    n: u64,
    timer: Option<PolledTimer>,
    driver: &'a dyn Driver,
}

impl<'a> Backoff<'a> {
    /// Returns a new backoff object, usable for the lifetime of one spin loop.
    pub fn new(driver: &'a dyn Driver) -> Self {
        Self {
            n: 0,
            timer: None,
            driver,
        }
    }

    /// Yields execution.
    ///
    /// Initially just yields to any other pending tasks. Yields for longer the
    /// more times this is called.
    pub async fn back_off(&mut self) {
        if self.n < 250 {
            yield_once().await;
        } else {
            let delay = if self.n - YIELD_ATTEMPTS < SHORT_SLEEP_ATTEMPTS {
                SHORT_SLEEP_DURATION
            } else {
                LONG_SLEEP_DURATION
            };
            self.timer
                .get_or_insert_with(|| PolledTimer::new(self.driver))
                .sleep(delay)
                .await;
        }
        self.n += 1;
    }
}

fn yield_once() -> YieldOnce {
    YieldOnce { yielded: false }
}

struct YieldOnce {
    yielded: bool,
}

impl Future for YieldOnce {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.yielded {
            self.yielded = true;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        ().into()
    }
}
