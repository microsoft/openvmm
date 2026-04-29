// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Flush sequencer — FSN tracking and concurrent flush coalescing.
//!
//! The VHDX write path needs a way to order and coalesce file flush operations.
//! Multiple concurrent callers may request flushes simultaneously. Rather than
//! issuing one file flush per caller, the [`FlushSequencer`] coalesces them: if
//! a flush is already in progress that will satisfy a caller's flush sequence
//! number (FSN), the caller waits for that flush instead of issuing a new one.
//!
//! FSNs increase monotonically. Each `flush()` call is assigned the next FSN.
//! When the flush I/O completes, the completed FSN advances to match. Callers
//! can ensure all data through a specific FSN is flushed via
//! [`FlushSequencer::flush_through`].

#![allow(dead_code)]

use crate::AsyncFile;
use crate::open::FailureFlag;
use event_listener::Event;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Acquire;
use std::sync::atomic::Ordering::Release;

/// Tracks flush sequence numbers and coalesces concurrent flush requests.
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
    issued_fsn: Fsn,
    completed_fsn: Fsn,
    active_flush: Option<Arc<Flush>>,
}

struct Flush {
    fsn: Fsn,
    done: AtomicBool,
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
    pub fn current_fsn(&self) -> Fsn {
        let state = self.state.lock();
        Fsn(state.issued_fsn.0 + 1)
    }

    /// Request a file flush through the sequencer.
    pub async fn flush(&self, file: &impl AsyncFile) -> Result<Fsn, std::io::Error> {
        self.flush_until(file, None).await
    }

    /// Ensure all data through the given FSN is durably flushed.
    pub async fn flush_through(
        &self,
        file: &impl AsyncFile,
        fsn: Fsn,
    ) -> Result<(), std::io::Error> {
        let completed = self.flush_until(file, Some(fsn)).await?;
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

    async fn flush_until(
        &self,
        file: &impl AsyncFile,
        mut requested_fsn: Option<Fsn>,
    ) -> Result<Fsn, std::io::Error> {
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
        r.inspect_err(|err| {
            if let Some(flag) = &self.failure_flag {
                flag.set(err);
            }
        })?;
        Ok(completed_fsn)
    }
}

impl Flush {
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

    #[async_test]
    async fn test_basic_flush() {
        let file = InMemoryFile::new(0);
        let seq = FlushSequencer::new();
        let fsn = seq.flush(&file).await.unwrap();
        assert_eq!(fsn, Fsn::new(1));
        assert_eq!(seq.completed_fsn(), Fsn::new(1));
    }

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

        assert!((Fsn::new(1)..=Fsn::new(2)).contains(&fsn1));
        assert!((Fsn::new(1)..=Fsn::new(2)).contains(&fsn2));
        assert_ne!(fsn1, fsn2);
        assert!(seq.completed_fsn() >= fsn1.max(fsn2));
        assert!(file.flush_count() <= 2);
    }

    #[async_test]
    async fn test_flush_through_already_completed() {
        let file = CountingFile::new();
        let seq = FlushSequencer::new();
        let fsn = seq.flush(&file).await.unwrap();
        let count_before = file.flush_count();
        seq.flush_through(&file, fsn).await.unwrap();
        assert_eq!(seq.completed_fsn(), fsn);
        assert_eq!(file.flush_count(), count_before);
    }

    #[async_test]
    async fn test_flush_through_triggers_flush() {
        let file = CountingFile::new();
        let seq = FlushSequencer::new();
        seq.flush_through(&file, Fsn::new(1)).await.unwrap();
        assert!(seq.completed_fsn() >= Fsn::new(1));
        assert!(file.flush_count() >= 1);
    }

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

    #[async_test]
    async fn test_flush_error_propagated() {
        let file = FailingFile::new(true);
        let seq = FlushSequencer::new();
        let result = seq.flush(&file).await;
        assert!(result.is_err());
        assert_eq!(seq.completed_fsn(), Fsn::ZERO);
    }

    #[async_test]
    async fn test_flush_error_recovery() {
        let file = FailingFile::new(true);
        let seq = FlushSequencer::new();

        assert!(seq.flush(&file).await.is_err());
        assert_eq!(seq.completed_fsn(), Fsn::ZERO);

        file.set_fail(false);
        let fsn = seq.flush(&file).await.unwrap();
        assert_eq!(fsn, Fsn::new(2));
        assert_eq!(seq.completed_fsn(), Fsn::new(2));
    }
}
