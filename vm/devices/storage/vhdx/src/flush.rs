// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Flush sequencer — FSN tracking and concurrent flush coalescing.
//!
//! The VHDX write path needs a way to order and coalesce file flush operations.
//! Multiple concurrent callers may request flushes simultaneously (e.g., several
//! I/O threads completing writes at the same time). Rather than issuing one file
//! flush per caller, the [`FlushSequencer`] coalesces them: if a flush is already
//! in progress that will satisfy a caller's flush sequence number (FSN), the
//! caller waits for that flush instead of issuing a new one.
//!
//! FSNs increase monotonically. Each `flush()` call is assigned the next FSN.
//! When the flush I/O completes, the completed FSN advances to match. Callers
//! can ensure all data through a specific FSN is flushed via
//! [`FlushSequencer::flush_through`].
//!
//! Used by both the [log task](crate::log_task) (to make WAL entries durable
//! before advancing the log tail) and
//! [`VhdxFile::flush()`](crate::open::VhdxFile::flush) (for crash safety).

use crate::AsyncFile;
use crate::open::FailureFlag;
use event_listener::Event;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Acquire;
use std::sync::atomic::Ordering::Release;

/// Tracks flush sequence numbers and coalesces concurrent flush requests.
///
/// Multiple callers can request flushes concurrently. The sequencer ensures
/// that at most one file flush is in progress at a time. If a flush is
/// in-flight that will satisfy a caller's FSN, the caller waits for that
/// flush instead of issuing a redundant one.
///
/// FSNs increase monotonically. Each [`flush()`](FlushSequencer::flush) call
/// is assigned the next FSN. [`flush_through()`](FlushSequencer::flush_through)
/// ensures all data through a specific FSN is flushed (used by the log task
/// to enforce ordering constraints like "data must be flushed before BAT
/// is logged").
pub(crate) struct FlushSequencer {
    state: Mutex<FlushState>,
    failure_flag: Option<Arc<FailureFlag>>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Fsn(u64);

impl Fsn {
    pub const ZERO: Self = Fsn(0);

    #[cfg(test)]
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

struct FlushState {
    /// The most recently issued FSN that has been assigned. The next flush
    /// will get `issued_fsn + 1`.
    issued_fsn: Fsn,
    /// The most recently completed FSN. All FSNs <= this value have been
    /// durably flushed.
    completed_fsn: Fsn,
    /// The currently in-progress flush, if any. New callers whose target FSN
    /// is covered by this flush wait on it rather than issuing a redundant
    /// flush. If a caller needs a higher FSN, it replaces this with a new
    /// flush (the old one continues running but is no longer advertised).
    active_flush: Option<Arc<Flush>>,
}

/// A single in-progress flush operation. Waiters hold an `Arc` clone and
/// poll [`wait_done`](Flush::wait_done) until the flusher signals completion.
struct Flush {
    /// The FSN that this flush will satisfy when it completes.
    fsn: Fsn,
    /// Set to `true` by the flusher after the I/O completes (success or failure).
    done: AtomicBool,
    /// Notified when `done` becomes `true`.
    event: Event,
}

impl FlushSequencer {
    /// Create a new flush sequencer with FSNs starting at 0.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(FlushState {
                issued_fsn: Fsn::ZERO,
                completed_fsn: Fsn::ZERO,
                active_flush: None,
            }),
            failure_flag: None,
        }
    }

    /// Set the failure flag for poisoning on I/O errors.
    pub fn set_failure_flag(&mut self, flag: Arc<FailureFlag>) {
        self.failure_flag = Some(flag);
    }

    /// Returns the next FSN that will be assigned to a flush request.
    ///
    /// This is `issued_fsn + 1`. Callers use this to capture the "current
    /// point in time" after performing writes, so they can later
    /// [`flush_through()`](Self::flush_through) to ensure those writes have
    /// been flushed.
    ///
    /// # Why this is safe without "reserving" an FSN
    ///
    /// Because `completed_fsn ≤ issued_fsn` always holds, `current_fsn()`
    /// always returns a value *strictly greater than* `completed_fsn`.
    /// Therefore `flush_through(current_fsn())` will never short-circuit
    /// — it always creates a new `Flush` (or waits for one with a
    /// sufficiently high FSN) that calls `file.flush()` *after* the
    /// caller's writes have been issued.  No separate reserve/issue
    /// step is needed.
    pub fn current_fsn(&self) -> Fsn {
        let state = self.state.lock();
        Fsn(state.issued_fsn.0 + 1)
    }

    /// Request a file flush through the sequencer.
    ///
    /// Assigns the next FSN to this flush request and ensures that a file
    /// flush completes that covers this FSN. Multiple concurrent `flush()`
    /// calls are coalesced: if a flush is already in progress, the caller
    /// waits for it to complete. If the completed FSN is still less than the
    /// caller's FSN after the in-progress flush finishes, a new flush is
    /// issued.
    ///
    /// Returns the completed FSN of the flush that satisfied this request.
    /// Under concurrency this may be greater than the FSN originally assigned
    /// to this caller, because a single flush can cover multiple requests.
    pub async fn flush(&self, file: &impl AsyncFile) -> Result<Fsn, std::io::Error> {
        self.flush_until(file, None).await
    }

    /// Ensure all data through the given FSN is durably flushed.
    ///
    /// If the FSN has already completed, returns immediately. Otherwise,
    /// bumps `issued_fsn` if needed and waits for a flush to complete that
    /// covers the requested FSN.
    ///
    /// This is the safe replacement for the old `require_fsn` + `wait_for_fsn`
    /// pattern — it both issues and waits in a single call.
    pub async fn flush_through(
        &self,
        file: &impl AsyncFile,
        fsn: Fsn,
    ) -> Result<(), std::io::Error> {
        let completed = self.flush_until(file, Some(fsn)).await?;
        // Safety invariant: callers only pass FSNs from current_fsn(), which is
        // issued_fsn + 1 at capture time. Since issued_fsn only grows, by the
        // time we run, issued_fsn + 1 >= fsn, so the flush we create always
        // covers the target. If this fires, a caller passed a bogus FSN.
        assert!(
            completed >= fsn,
            "flush_through({fsn:?}) completed only through {completed:?}"
        );
        Ok(())
    }

    /// Returns the most recently completed FSN.
    pub fn completed_fsn(&self) -> Fsn {
        self.state.lock().completed_fsn
    }

    /// Inner workhorse: keep flushing until `completed_fsn >= target_fsn`.
    ///
    /// `target_fsn`:
    /// - `None` — assign the next sequential FSN (used by `flush()`).
    /// - `Some(fsn)` — ensure completion through that FSN (used by `flush_through()`).
    ///
    /// Returns the resolved FSN.
    async fn flush_until(
        &self,
        file: &impl AsyncFile,
        mut requested_fsn: Option<Fsn>,
    ) -> Result<Fsn, std::io::Error> {
        // Step 1: find or create the Flush we'll execute.
        //
        // If there's an active flush covering our target FSN, wait for it.
        // When it completes, loop back — if completed_fsn >= target we're
        // done; otherwise we'll create a new flush ourselves.
        //
        // If no active flush covers our target, create one and `break` out
        // of the loop to proceed to the I/O in step 2.
        let my_flush = loop {
            let active = {
                let mut state = self.state.lock();
                let target_fsn = requested_fsn.unwrap_or(Fsn(state.issued_fsn.0 + 1));
                requested_fsn = Some(target_fsn);

                if target_fsn <= state.completed_fsn {
                    return Ok(state.completed_fsn);
                }

                if let Some(active) = &state.active_flush
                    && active.fsn >= target_fsn
                {
                    active.clone()
                } else {
                    let fsn = Fsn(state.issued_fsn.0 + 1);
                    // flush_through() callers capture FSNs via current_fsn(),
                    // which returns issued_fsn + 1. Since issued_fsn only
                    // grows, the target can never exceed the next FSN we're
                    // about to issue.
                    assert!(
                        target_fsn <= fsn,
                        "flush_through target {target_fsn:?} exceeds next FSN {fsn:?}"
                    );
                    let new_flush = Arc::new(Flush {
                        fsn,
                        done: false.into(),
                        event: Default::default(),
                    });
                    state.active_flush = Some(new_flush.clone());
                    state.issued_fsn = fsn;
                    break new_flush;
                }
            };
            active.wait_done().await;
        };

        // Step 2: perform the actual file flush and update state.
        let r = file.flush().await;
        let completed_fsn = {
            let mut state = self.state.lock();
            if r.is_ok() {
                state.completed_fsn = my_flush.fsn.max(state.completed_fsn);
            }
            if state
                .active_flush
                .as_ref()
                .is_some_and(|p| Arc::ptr_eq(p, &my_flush))
            {
                state.active_flush = None;
            }
            state.completed_fsn
        };
        my_flush.done.store(true, Release);
        my_flush.event.notify(usize::MAX);
        r.inspect_err(|e| {
            if let Some(flag) = &self.failure_flag {
                flag.set(e);
            }
        })?;
        Ok(completed_fsn)
    }
}

impl Flush {
    /// Wait for this flush to complete (success or failure).
    async fn wait_done(&self) {
        loop {
            let event = self.event.listen();
            if self.done.load(Acquire) {
                break;
            }
            event.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::support::InMemoryFile;
    use pal_async::async_test;
    use std::borrow::Borrow;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    // -- Helper wrappers --

    /// File wrapper that counts how many times `flush()` is called.
    struct CountingFile {
        inner: InMemoryFile,
        flush_count: AtomicU32,
    }

    impl CountingFile {
        fn new() -> Self {
            Self {
                inner: InMemoryFile::new(0),
                flush_count: AtomicU32::new(0),
            }
        }

        fn flush_count(&self) -> u32 {
            self.flush_count.load(Ordering::Relaxed)
        }
    }

    impl AsyncFile for CountingFile {
        type Buffer = Vec<u8>;

        fn alloc_buffer(&self, len: usize) -> Vec<u8> {
            self.inner.alloc_buffer(len)
        }

        async fn read_into(&self, offset: u64, buf: Vec<u8>) -> Result<Vec<u8>, std::io::Error> {
            self.inner.read_into(offset, buf).await
        }

        async fn write_from(
            &self,
            offset: u64,
            buf: impl Borrow<Vec<u8>> + Send + 'static,
        ) -> Result<(), std::io::Error> {
            self.inner.write_from(offset, buf).await
        }

        async fn flush(&self) -> Result<(), std::io::Error> {
            self.flush_count.fetch_add(1, Ordering::Relaxed);
            self.inner.flush().await
        }

        async fn file_size(&self) -> Result<u64, std::io::Error> {
            self.inner.file_size().await
        }

        async fn set_file_size(&self, size: u64) -> Result<(), std::io::Error> {
            self.inner.set_file_size(size).await
        }
    }

    /// File wrapper that can be configured to fail flushes.
    struct FailingFile {
        inner: InMemoryFile,
        fail_flush: AtomicBool,
    }

    impl FailingFile {
        fn new(fail: bool) -> Self {
            Self {
                inner: InMemoryFile::new(0),
                fail_flush: AtomicBool::new(fail),
            }
        }

        fn set_fail(&self, fail: bool) {
            self.fail_flush.store(fail, Ordering::Relaxed);
        }
    }

    impl AsyncFile for FailingFile {
        type Buffer = Vec<u8>;

        fn alloc_buffer(&self, len: usize) -> Vec<u8> {
            self.inner.alloc_buffer(len)
        }

        async fn read_into(&self, offset: u64, buf: Vec<u8>) -> Result<Vec<u8>, std::io::Error> {
            self.inner.read_into(offset, buf).await
        }

        async fn write_from(
            &self,
            offset: u64,
            buf: impl Borrow<Vec<u8>> + Send + 'static,
        ) -> Result<(), std::io::Error> {
            self.inner.write_from(offset, buf).await
        }

        async fn flush(&self) -> Result<(), std::io::Error> {
            if self.fail_flush.load(Ordering::Relaxed) {
                return Err(std::io::Error::other("flush failed"));
            }
            self.inner.flush().await
        }

        async fn file_size(&self) -> Result<u64, std::io::Error> {
            self.inner.file_size().await
        }

        async fn set_file_size(&self, size: u64) -> Result<(), std::io::Error> {
            self.inner.set_file_size(size).await
        }
    }

    // -- Tests --

    /// Single `flush()` call → FSN advances from 0 to 1.
    #[async_test]
    async fn test_basic_flush() {
        let file = InMemoryFile::new(0);
        let seq = FlushSequencer::new();
        let fsn = seq.flush(&file).await.unwrap();
        assert_eq!(fsn, Fsn::new(1));
        assert_eq!(seq.completed_fsn(), Fsn::new(1));
    }

    /// Three sequential `flush()` calls → FSNs are 1, 2, 3.
    #[async_test]
    async fn test_fsn_monotonically_increasing() {
        let file = InMemoryFile::new(0);
        let seq = FlushSequencer::new();
        let fsn1 = seq.flush(&file).await.unwrap();
        let fsn2 = seq.flush(&file).await.unwrap();
        let fsn3 = seq.flush(&file).await.unwrap();
        assert_eq!(fsn1, Fsn::new(1));
        assert_eq!(fsn2, Fsn::new(2));
        assert_eq!(fsn3, Fsn::new(3));
        assert_eq!(seq.completed_fsn(), Fsn::new(3));
    }

    /// `current_fsn()` returns 1 initially, advances after each flush.
    #[async_test]
    async fn test_current_fsn() {
        let file = InMemoryFile::new(0);
        let seq = FlushSequencer::new();
        assert_eq!(seq.current_fsn(), Fsn::new(1));
        seq.flush(&file).await.unwrap();
        assert_eq!(seq.current_fsn(), Fsn::new(2));
        seq.flush(&file).await.unwrap();
        assert_eq!(seq.current_fsn(), Fsn::new(3));
    }

    /// Spawn two concurrent `flush()` tasks. Both should complete, and the
    /// total number of actual file flushes should be ≤ 2 (possibly 1 if
    /// coalesced).
    #[async_test]
    async fn test_concurrent_flush_coalescing() {
        let file = Arc::new(CountingFile::new());
        let seq = Arc::new(FlushSequencer::new());

        let file1 = file.clone();
        let seq1 = seq.clone();
        let t1 =
            futures::FutureExt::boxed(async move { seq1.flush(file1.as_ref()).await.unwrap() });

        let file2 = file.clone();
        let seq2 = seq.clone();
        let t2 =
            futures::FutureExt::boxed(async move { seq2.flush(file2.as_ref()).await.unwrap() });

        let (fsn1, fsn2) = futures::join!(t1, t2);

        // Both FSNs should be valid (1 or 2).
        assert!((Fsn::new(1)..=Fsn::new(2)).contains(&fsn1));
        assert!((Fsn::new(1)..=Fsn::new(2)).contains(&fsn2));
        assert_ne!(fsn1, fsn2);

        // Completed FSN should be at least the max of both.
        assert!(seq.completed_fsn() >= fsn1.max(fsn2));

        // At most 2 actual file flushes should have occurred.
        assert!(file.flush_count() <= 2);
    }

    /// Call `flush()`, then `flush_through(fsn)` → returns immediately.
    #[async_test]
    async fn test_flush_through_already_completed() {
        let file = CountingFile::new();
        let seq = FlushSequencer::new();
        let fsn = seq.flush(&file).await.unwrap();
        let count_before = file.flush_count();
        // Should return immediately since the FSN is already completed.
        seq.flush_through(&file, fsn).await.unwrap();
        assert_eq!(seq.completed_fsn(), fsn);
        // No additional flush should have been issued.
        assert_eq!(file.flush_count(), count_before);
    }

    /// Call `flush_through(fsn)` on an un-issued FSN → triggers a flush
    /// and completes.
    #[async_test]
    async fn test_flush_through_triggers_flush() {
        let file = CountingFile::new();
        let seq = FlushSequencer::new();
        // FSN 1 has not been issued yet.
        seq.flush_through(&file, Fsn::new(1)).await.unwrap();
        assert!(seq.completed_fsn() >= Fsn::new(1));
        assert!(file.flush_count() >= 1);
    }

    /// Spawn a concurrent `flush()` and `flush_through()` — both complete.
    #[async_test]
    async fn test_flush_through_waits_for_in_progress() {
        let file = Arc::new(CountingFile::new());
        let seq = Arc::new(FlushSequencer::new());

        let file1 = file.clone();
        let seq1 = seq.clone();
        let flusher = futures::FutureExt::boxed(async move {
            seq1.flush(file1.as_ref()).await.unwrap();
        });

        let file2 = file.clone();
        let seq2 = seq.clone();
        let waiter = futures::FutureExt::boxed(async move {
            seq2.flush_through(file2.as_ref(), Fsn::new(1))
                .await
                .unwrap();
        });

        futures::join!(flusher, waiter);
        assert!(seq.completed_fsn() >= Fsn::new(1));
    }

    /// Use a file wrapper that fails on `flush()` → `flush()` returns error,
    /// `completed_fsn` does NOT advance.
    #[async_test]
    async fn test_flush_error_propagated() {
        let file = FailingFile::new(true);
        let seq = FlushSequencer::new();
        let result = seq.flush(&file).await;
        assert!(result.is_err());
        assert_eq!(seq.completed_fsn(), Fsn::ZERO);
    }

    /// Use a file wrapper that fails on the first `flush()` but succeeds on
    /// retry → first call fails, second `flush()` succeeds and FSN advances.
    #[async_test]
    async fn test_flush_error_recovery() {
        let file = FailingFile::new(true);
        let seq = FlushSequencer::new();

        // First flush should fail.
        let result = seq.flush(&file).await;
        assert!(result.is_err());
        assert_eq!(seq.completed_fsn(), Fsn::ZERO);

        // Allow flushes to succeed now.
        file.set_fail(false);

        // Second flush should succeed.
        let fsn = seq.flush(&file).await.unwrap();
        assert!(fsn >= Fsn::new(1));
        assert!(seq.completed_fsn() >= fsn);
    }

    /// `flush_through(0)` returns immediately — FSN 0 is always completed
    /// since the sequencer starts with `completed_fsn = 0`.
    #[async_test]
    async fn test_flush_through_zero_is_noop() {
        let file = CountingFile::new();
        let seq = FlushSequencer::new();
        seq.flush_through(&file, Fsn::ZERO).await.unwrap();
        assert_eq!(file.flush_count(), 0);
        assert_eq!(seq.completed_fsn(), Fsn::ZERO);
    }

    /// `flush_through` on a failing file propagates the error, and a
    /// subsequent retry with a working file succeeds.
    #[async_test]
    async fn test_flush_through_error_recovery() {
        let file = FailingFile::new(true);
        let seq = FlushSequencer::new();

        let result = seq.flush_through(&file, Fsn::new(1)).await;
        assert!(result.is_err());
        assert_eq!(seq.completed_fsn(), Fsn::ZERO);

        file.set_fail(false);
        seq.flush_through(&file, Fsn::new(1)).await.unwrap();
        assert!(seq.completed_fsn() >= Fsn::new(1));
    }

    /// Two concurrent `flush_through` calls for the same FSN — both
    /// complete, and the total number of file flushes is reasonable.
    #[async_test]
    async fn test_concurrent_flush_through_same_fsn() {
        let file = Arc::new(CountingFile::new());
        let seq = Arc::new(FlushSequencer::new());

        let file1 = file.clone();
        let seq1 = seq.clone();
        let t1 = futures::FutureExt::boxed(async move {
            seq1.flush_through(file1.as_ref(), Fsn::new(1))
                .await
                .unwrap();
        });

        let file2 = file.clone();
        let seq2 = seq.clone();
        let t2 = futures::FutureExt::boxed(async move {
            seq2.flush_through(file2.as_ref(), Fsn::new(1))
                .await
                .unwrap();
        });

        futures::join!(t1, t2);
        assert!(seq.completed_fsn() >= Fsn::new(1));
    }

    /// `flush()` returns `completed_fsn`, which may be higher than the
    /// caller's own FSN if a concurrent flush for a higher FSN completed.
    #[async_test]
    async fn test_flush_returns_completed_fsn() {
        let file = InMemoryFile::new(0);
        let seq = FlushSequencer::new();

        // Sequential flushes: returned FSN equals completed_fsn.
        let fsn = seq.flush(&file).await.unwrap();
        assert_eq!(fsn, seq.completed_fsn());
    }
}
