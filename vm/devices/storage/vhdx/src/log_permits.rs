// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Failable semaphore for log pipeline backpressure.
//!
//! [`LogPermits`] limits how many pages can be in-flight in the
//! cache → log → apply pipeline at once. This bounds memory
//! consumption: each in-flight page holds an `Arc<[u8; 4096]>`
//! that cannot be freed until the apply task writes it to its
//! final file offset.
//!
//! **Lifecycle of a permit:**
//! 1. Cache acquires a permit before transitioning a page to
//!    `HasPermit` / `Dirty`.
//! 2. The permit stays consumed through commit → log → apply.
//! 3. The apply task releases the permit after writing the page
//!    to its final offset.
//!
//! If the log task fails, the semaphore is **poisoned** — all
//! pending and future acquires return an error.

use crate::error::PipelineFailed;
use event_listener::Event;
use parking_lot::Mutex;

/// Failable semaphore shared between the cache and the apply task.
///
/// The cache acquires permits before dirtying pages. The **apply task**
/// releases permits after writing pages to their final file offsets.
/// Do NOT release permits at commit time — that defeats backpressure
/// and allows unbounded in-flight allocations.
///
/// If the log task fails, it poisons the semaphore — all waiters and
/// future callers get errors.
pub(crate) struct LogPermits {
    state: Mutex<PermitState>,
    event: Event,
    pressure_event: Event,
    max_permits: usize,
}

struct PermitState {
    available: usize,
    failed: Option<String>,
    blocked_requests: Vec<usize>,
}

impl PermitState {
    fn remove_blocked_request(&mut self, count: usize) {
        let index = self
            .blocked_requests
            .iter()
            .position(|&request| request == count)
            .expect("blocked permit request disappeared");
        self.blocked_requests.swap_remove(index);
    }
}

struct PermitWaiter<'a> {
    permits: &'a LogPermits,
    count: usize,
    registered: bool,
}

impl Drop for PermitWaiter<'_> {
    fn drop(&mut self) {
        if self.registered {
            self.permits.state.lock().remove_blocked_request(self.count);
        }
    }
}

impl LogPermits {
    /// Create a new semaphore with `max_in_flight` permits.
    pub fn new(max_in_flight: usize) -> Self {
        Self {
            state: Mutex::new(PermitState {
                available: max_in_flight,
                failed: None,
                blocked_requests: Vec::new(),
            }),
            event: Event::new(),
            pressure_event: Event::new(),
            max_permits: max_in_flight,
        }
    }

    /// Acquire `count` permits.
    ///
    /// Blocks if insufficient permits are available. Returns an error
    /// if the semaphore has been poisoned.
    pub async fn acquire(&self, count: usize) -> Result<(), PipelineFailed> {
        let mut waiter = PermitWaiter {
            permits: self,
            count,
            registered: false,
        };
        loop {
            let listener = self.event.listen();
            let (acquired, pressure) = {
                let mut state = self.state.lock();
                if let Some(ref err) = state.failed {
                    return Err(PipelineFailed(err.clone()));
                }
                let acquired = if state.available >= count {
                    if waiter.registered {
                        state.remove_blocked_request(count);
                        waiter.registered = false;
                    }
                    state.available -= count;
                    true
                } else {
                    if !waiter.registered {
                        state.blocked_requests.push(count);
                        waiter.registered = true;
                    }
                    false
                };
                (acquired, !state.blocked_requests.is_empty())
            };
            if pressure {
                self.pressure_event.notify(usize::MAX);
            }
            if acquired {
                return Ok(());
            }
            listener.await;
        }
    }

    /// Wait for an acquisition that cannot be satisfied by available permits.
    ///
    /// Demand remains visible until acquisition or cancellation, even when
    /// it precedes the WAL batch that can release the necessary permits.
    pub async fn wait_for_pressure(&self) -> Result<(), PipelineFailed> {
        loop {
            let listener = self.pressure_event.listen();
            {
                let state = self.state.lock();
                if let Some(error) = &state.failed {
                    return Err(PipelineFailed(error.clone()));
                }
                if state
                    .blocked_requests
                    .iter()
                    .any(|&count| count > state.available)
                {
                    return Ok(());
                }
            }
            listener.await;
        }
    }

    /// Release `count` permits back to the pool.
    ///
    /// Called by the apply task after writing pages to their final offsets.
    pub fn release(&self, count: usize) {
        {
            let mut state = self.state.lock();
            state.available += count;
            assert!(
                state.available <= self.max_permits,
                "released more permits than were acquired: available {} > max {}",
                state.available,
                self.max_permits,
            );
        }
        self.event.notify(usize::MAX);
        self.pressure_event.notify(usize::MAX);
    }

    /// Poison the semaphore. All pending and future acquires will fail.
    ///
    /// Called by the log task on error.
    pub fn fail(&self, error: String) {
        {
            let mut state = self.state.lock();
            state.failed = Some(error);
        }
        self.event.notify(usize::MAX);
        self.pressure_event.notify(usize::MAX);
    }

    /// Returns the number of currently available permits.
    #[cfg(test)]
    pub fn available(&self) -> usize {
        self.state.lock().available
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pal_async::async_test;
    use test_with_tracing::test;

    #[async_test]
    async fn acquire_and_release() {
        let permits = LogPermits::new(10);
        permits.acquire(3).await.unwrap();
        assert_eq!(permits.available(), 7);
        permits.release(3);
        assert_eq!(permits.available(), 10);
    }

    #[async_test]
    async fn acquire_exact_capacity() {
        let permits = LogPermits::new(5);
        permits.acquire(5).await.unwrap();
        assert_eq!(permits.available(), 0);
        permits.release(5);
        assert_eq!(permits.available(), 5);
    }

    #[async_test]
    async fn acquire_blocks_then_unblocks() {
        let permits = std::sync::Arc::new(LogPermits::new(2));
        permits.acquire(2).await.unwrap();
        assert_eq!(permits.available(), 0);

        let p = permits.clone();
        let (acquired_tx, acquired_rx) = mesh::oneshot();
        let handle = std::thread::spawn(move || {
            futures::executor::block_on(async {
                p.acquire(1).await.unwrap();
                acquired_tx.send(());
            });
        });

        // Give the thread time to block on acquire.
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Should still be blocked (0 available).
        assert_eq!(permits.available(), 0);

        // Release one permit — unblocks the waiter.
        permits.release(1);
        acquired_rx.await.unwrap();
        handle.join().unwrap();
    }

    #[async_test]
    async fn poison_fails_pending_acquire() {
        let permits = std::sync::Arc::new(LogPermits::new(0));

        let p = permits.clone();
        let handle = std::thread::spawn(move || {
            futures::executor::block_on(async {
                let result = p.acquire(1).await;
                assert!(result.is_err());
            });
        });

        // Give the thread time to block.
        std::thread::sleep(std::time::Duration::from_millis(50));
        permits.fail("log write failed".into());
        handle.join().unwrap();
    }

    #[async_test]
    async fn poison_fails_future_acquire() {
        let permits = LogPermits::new(10);
        permits.fail("log write failed".into());
        let result = permits.acquire(1).await;
        assert!(result.is_err());
    }

    #[async_test]
    async fn release_after_poison_is_harmless() {
        let permits = LogPermits::new(5);
        permits.acquire(3).await.unwrap();
        permits.fail("oops".into());
        // Release after poison doesn't panic.
        permits.release(3);
        // But acquire still fails.
        assert!(permits.acquire(1).await.is_err());
    }

    #[async_test]
    async fn multiple_acquires_serialize() {
        let permits = LogPermits::new(3);
        permits.acquire(2).await.unwrap();
        permits.acquire(1).await.unwrap();
        assert_eq!(permits.available(), 0);
        permits.release(1);
        permits.acquire(1).await.unwrap();
        assert_eq!(permits.available(), 0);
    }

    #[async_test]
    async fn pressure_tracks_identical_waiters_and_cancellation() {
        let permits = LogPermits::new(4);
        permits.acquire(2).await.unwrap();
        let mut remaining = std::pin::pin!(permits.acquire(3));
        {
            let mut cancelled = std::pin::pin!(permits.acquire(3));
            assert!(futures::poll!(&mut cancelled).is_pending());
            assert!(futures::poll!(&mut remaining).is_pending());
        }
        let mut pressure = std::pin::pin!(permits.wait_for_pressure());
        assert!(matches!(
            futures::poll!(&mut pressure),
            std::task::Poll::Ready(Ok(()))
        ));
        permits.release(2);
        let mut pressure = std::pin::pin!(permits.wait_for_pressure());
        assert!(futures::poll!(&mut pressure).is_pending());
        assert!(matches!(
            futures::poll!(&mut remaining),
            std::task::Poll::Ready(Ok(()))
        ));
        assert!(futures::poll!(&mut pressure).is_pending());
        assert_eq!(permits.available(), 1);
        permits.release(3);
    }

    #[async_test]
    async fn pressure_wakes_when_availability_falls_before_waiter_retries() {
        let permits = LogPermits::new(4);
        permits.acquire(2).await.unwrap();
        let mut acquire = std::pin::pin!(permits.acquire(3));
        assert!(futures::poll!(&mut acquire).is_pending());
        permits.release(2);
        let mut pressure = std::pin::pin!(permits.wait_for_pressure());
        assert!(futures::poll!(&mut pressure).is_pending());

        permits.acquire(2).await.unwrap();
        assert!(matches!(
            futures::poll!(&mut pressure),
            std::task::Poll::Ready(Ok(()))
        ));
        permits.release(2);
        assert!(matches!(
            futures::poll!(&mut acquire),
            std::task::Poll::Ready(Ok(()))
        ));
        assert_eq!(permits.available(), 1);
        permits.release(3);
    }

    #[async_test]
    async fn failure_wakes_pressure_observer() {
        let permits = LogPermits::new(4);
        let mut pressure = std::pin::pin!(permits.wait_for_pressure());
        assert!(futures::poll!(&mut pressure).is_pending());
        permits.fail("injected pipeline failure".into());
        assert!(matches!(
            futures::poll!(&mut pressure),
            std::task::Poll::Ready(Err(_))
        ));
    }
}
