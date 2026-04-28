// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Failable semaphore for log pipeline backpressure.

#![allow(dead_code)]

use crate::error::PipelineFailed;
use event_listener::Event;
use parking_lot::Mutex;

/// Failable semaphore shared between the cache and the apply task.
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

    /// Poison the semaphore.
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
    use std::sync::Arc;
    use std::sync::mpsc;

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
        let permits = Arc::new(LogPermits::new(2));
        permits.acquire(2).await.unwrap();
        assert_eq!(permits.available(), 0);

        let p = permits.clone();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            futures::executor::block_on(async {
                p.acquire(1).await.unwrap();
                acquired_tx.send(()).unwrap();
            });
        });

        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(permits.available(), 0);

        permits.release(1);
        acquired_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        handle.join().unwrap();
    }

    #[async_test]
    async fn poison_fails_pending_acquire() {
        let permits = Arc::new(LogPermits::new(0));

        let p = permits.clone();
        let handle = std::thread::spawn(move || {
            futures::executor::block_on(async {
                let result = p.acquire(1).await;
                assert!(result.is_err());
            });
        });

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
        permits.release(3);
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
