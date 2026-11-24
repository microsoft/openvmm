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
//!    to its final offset and flushing.
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
    max_permits: usize,
}

struct PermitState {
    available: usize,
    failed: Option<String>,
}

impl LogPermits {
    /// Create a new semaphore with `max_in_flight` permits.
    pub fn new(max_in_flight: usize) -> Self {
        Self {
            state: Mutex::new(PermitState {
                available: max_in_flight,
                failed: None,
            }),
            event: Event::new(),
            max_permits: max_in_flight,
        }
    }

    /// Acquire `count` permits.
    ///
    /// Blocks if insufficient permits are available. Returns an error
    /// if the semaphore has been poisoned.
    pub async fn acquire(&self, count: usize) -> Result<(), PipelineFailed> {
        loop {
            let listener = self.event.listen();
            {
                let mut state = self.state.lock();
                if let Some(ref err) = state.failed {
                    return Err(PipelineFailed(err.clone()));
                }
                if state.available >= count {
                    state.available -= count;
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
}
