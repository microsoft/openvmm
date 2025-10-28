// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Minimal RCU (Read-Copy-Update) implementation
//!
//! This crate provides a minimal Read-Copy-Update (RCU) synchronization
//! mechanism specifically designed for OpenVMM use cases. RCU is a
//! synchronization technique that allows multiple readers to access shared data
//! concurrently with writers by ensuring that writers create new versions of
//! data while readers continue using old versions.
//!
//! This is similar to a reader-writer lock except that readers never wait:
//! writers publish the new version of the data and then wait for all readers to
//! finish using the old version before freeing it. This allows for very low
//! overhead on the read side, as readers do not need to acquire locks.
//!
//! ## Usage
//!
//! Basic usage with the global domain:
//!
//! ```rust
//! // Execute code in a read-side critical section
//! let result = minircu::global().run(|| {
//!     // Access shared data safely here.
//!     42
//! });
//!
//! // Wait for all current readers to finish their critical sections.
//! // This is typically called by writers after updating data.
//! minircu::global().synchronize_blocking();
//! ```
//!
//! ## Quiescing
//!
//! To optimize synchronization, threads can explicitly quiesce when it is not
//! expected to enter a critical section for a while. The RCU domain can skip
//! issuing a memory barrier when all threads are quiesced.
//!
//! ```rust
//! use minircu::global;
//!
//! // Mark the current thread as quiesced.
//! global().quiesce();
//! ```
//!
//! ## Asynchronous Support
//!
//! The crate provides async-compatible methods for quiescing and
//! synchronization:
//!
//! ```rust
//! use minircu::global;
//!
//! async fn example() {
//!     // Quiesce whenever future returns Poll::Pending
//!     global().quiesce_on_pending(async {
//!         loop {
//!             // Async code here.
//!             global().run(|| {
//!                 // Access shared data safely here.
//!             });
//!         }
//!     }).await;
//!
//!     // Asynchronous synchronization
//!     global().synchronize(|duration| async move {
//!         // This should be a sleep call, e.g. using tokio::time::sleep.
//!         std::future::pending().await
//!     }).await;
//! }
//! ```
//!
//! ## Gotchas
//!
//! * Avoid blocking or long-running operations in critical sections as they can
//!   delay writers or cause deadlocks.
//! * Never call [`synchronize`](RcuDomain::synchronize) or
//!   [`synchronize_blocking`](RcuDomain::synchronize_blocking) from within a critical
//!   section (will panic).
//! * For best performance, ensure all threads in your process call `quiesce`
//!   when a thread is going to sleep or block.
//!
//! ## Implementation Notes
//!
//! On Windows and Linux, the read-side critical section avoids any processor
//! memory barriers. It achieves this by having the write side broadcast a
//! memory barrier to all threads in the process when needed for
//! synchronization, via the `membarrier` syscall on Linux and
//! `FlushProcessWriteBuffers` on Windows.
//!
//! On other platforms, which do not support this functionality, the read-side
//! critical section uses a memory fence. This makes the read side more
//! expensive on these platforms, but it is still cheaper than a mutex or
//! reader-writer lock.

// UNSAFETY: needed to access TLS from a remote thread and to call platform APIs
// for issuing process-wide memory barriers.
#![expect(unsafe_code)]

/// Provides the environment-specific `membarrier` and `access_fence`
/// implementations.
#[cfg_attr(target_os = "linux", path = "linux.rs")]
#[cfg_attr(windows, path = "windows.rs")]
#[cfg_attr(not(any(windows, target_os = "linux")), path = "other.rs")]
mod sys;

use event_listener::Event;
use event_listener::Listener;
use parking_lot::Mutex;
use std::cell::Cell;
use std::future::Future;
use std::future::poll_fn;
use std::ops::Deref;
use std::pin::pin;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::Ordering::Release;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::fence;
use std::task::Poll;
use std::thread::LocalKey;
use std::thread::Thread;
use std::time::Duration;
use std::time::Instant;

/// Defines a new RCU domain, which can be synchronized with separately from
/// other domains.
///
/// Usually you just want to use [`global`], the global domain.
///
/// Don't export this until we have a use case. We may want to make `quiesce`
/// apply to all domains, or something like that.
macro_rules! define_rcu_domain {
    ($(#[$a:meta])* $vis:vis $name:ident) => {
        $(#[$a])*
        $vis const fn $name() -> $crate::RcuDomain {
            static DATA: $crate::RcuData = $crate::RcuData::new();
            thread_local! {
                static TLS: $crate::ThreadData = const { $crate::ThreadData::new() };
            }
            $crate::RcuDomain::new(&TLS, &DATA)
        }
    };
}

define_rcu_domain! {
    /// The global RCU domain.
    pub global
}

/// An RCU synchronization domain.
#[derive(Copy, Clone)]
pub struct RcuDomain {
    tls: &'static LocalKey<ThreadData>,
    data: &'static RcuData,
}

impl std::fmt::Debug for RcuDomain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { tls: _, data } = self;
        f.debug_struct("RcuDomain").field("data", data).finish()
    }
}

/// Domain-global RCU state.
#[doc(hidden)]
#[derive(Debug)]
pub struct RcuData {
    /// The threads that have registered with this domain.
    threads: Mutex<Vec<ThreadEntry>>,
    /// The current sequence number.
    seq: AtomicU64,
    /// The event that is signaled when a thread exits a critical section and
    /// there has been a sequence number update.
    event: Event,
    /// The number of membarriers issued.
    membarriers: AtomicU64,
}

/// The entry in the thread list for a registered thread.
#[derive(Debug)]
struct ThreadEntry {
    /// The pointer to the sequence number for this thread. The [`ThreadData`]
    /// TLS destructor will remove this entry, so this is safe to dereference.
    seq_ptr: TlsRef<AtomicU64>,
    /// The last sequence number that a synchronizer can know this thread has
    /// observed, without issuing membarriers or looking at the thread's TLS
    /// data.
    observed_seq: u64,
    /// The thread that this entry is for. Used for debugging and tracing.
    thread: Thread,
}

/// A pointer representing a valid reference to a value.
struct TlsRef<T>(*const T);

impl<T> Deref for TlsRef<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: This is known to point to valid TLS data for its lifetime, since the TLS
        // drop implementation will remove this entry from the list.
        unsafe { &*self.0 }
    }
}

// SAFETY: Since this represents a reference to T, it is `Send` if `&T` is
// `Send`.
unsafe impl<T> Send for TlsRef<T> where for<'a> &'a T: Send {}
// SAFETY: Since this represents a reference to T, it is `Sync` if `&T` is
// `Sync`.
unsafe impl<T> Sync for TlsRef<T> where for<'a> &'a T: Sync {}

impl<T: std::fmt::Debug> std::fmt::Debug for TlsRef<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (**self).fmt(f)
    }
}

impl RcuData {
    /// Used by [`define_rcu_domain!`] to create a new RCU domain.
    #[doc(hidden)]
    pub const fn new() -> Self {
        RcuData {
            threads: Mutex::new(Vec::new()),
            seq: AtomicU64::new(SEQ_FIRST),
            event: Event::new(),
            membarriers: AtomicU64::new(0),
        }
    }
}

/// The per-thread TLS data.
#[doc(hidden)]
pub struct ThreadData {
    /// The current sequence number for the thread.
    current_seq: AtomicU64,
    /// The RCU domain this thread is registered with.
    data: Cell<Option<&'static RcuData>>,
}

impl std::fmt::Debug for ThreadData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            current_seq: my_seq,
            data: _,
        } = self;
        f.debug_struct("ThreadData")
            .field("my_seq", my_seq)
            .finish()
    }
}

impl Drop for ThreadData {
    fn drop(&mut self) {
        if let Some(data) = self.data.get() {
            {
                let mut threads = data.threads.lock();
                let i = threads
                    .iter()
                    .position(|x| x.seq_ptr.0 == &self.current_seq)
                    .unwrap();
                threads.swap_remove(i);
            }
            data.event.notify(!0usize);
        }
    }
}

impl ThreadData {
    /// Used by [`define_rcu_domain!`] to create a new RCU domain.
    #[doc(hidden)]
    pub const fn new() -> Self {
        ThreadData {
            current_seq: AtomicU64::new(SEQ_NONE),
            data: Cell::new(None),
        }
    }
}

/// The thread has not yet registered with the RCU domain.
const SEQ_NONE: u64 = 0;
/// The bit set when the thread in a critical section.
const SEQ_MASK_BUSY: u64 = 1;
/// The value the sequence number is incremented by each synchronize call.
const SEQ_INCREMENT: u64 = 2;
/// The sequence value for a quiesced thread. The thread will issue a full
/// memory barrier when leaving this state.
const SEQ_QUIESCED: u64 = 2;
/// The first actual sequence number.
const SEQ_FIRST: u64 = 4;

impl RcuDomain {
    #[doc(hidden)]
    pub const fn new(tls: &'static LocalKey<ThreadData>, data: &'static RcuData) -> Self {
        RcuDomain { tls, data }
    }

    /// Runs `f` in a critical section. Calls to
    /// [`synchronize`](Self::synchronize) or
    /// [`synchronize_blocking`](Self::synchronize_blocking) for the same RCU root will
    /// block until `f` returns.
    ///
    /// In general, you should avoid blocking the thread in `f`, since that can
    /// slow calls to [`synchronize`](Self::synchronize) and can potentially
    /// cause deadlocks.
    pub fn run<F, R>(self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        self.tls.with(|x| {
            let _guard = x.enter(self.data);
            f()
        })
    }

    /// Quiesce the current thread.
    ///
    /// This can speed up calls to [`synchronize`](Self::synchronize) or
    /// [`synchronize_blocking`](Self::synchronize_blocking) by allowing the RCU domain
    /// to skip issuing a membarrier if all threads are quiesced. In return, the
    /// first call to [`run`](Self::run) after this will be slower, as it will
    /// need to issue a memory barrier to leave the quiesced state.
    pub fn quiesce(self) {
        self.tls.with(|x| {
            x.quiesce(self.data);
        });
    }

    /// Runs `fut`, calling [`quiesce`](Self::quiesce) on the current thread
    /// each time `fut` returns `Poll::Pending`.
    pub async fn quiesce_on_pending<Fut>(self, fut: Fut) -> Fut::Output
    where
        Fut: Future,
    {
        let mut fut = pin!(fut);
        poll_fn(|cx| {
            self.tls.with(|x| {
                let r = fut.as_mut().poll(cx);
                x.quiesce(self.data);
                r
            })
        })
        .await
    }

    #[track_caller]
    fn prepare_to_wait(&self) -> Option<u64> {
        // Quiesce this thread so we don't wait on ourselves.
        {
            let this_seq = self.tls.with(|x| x.quiesce(self.data));
            assert!(
                this_seq == SEQ_NONE || this_seq == SEQ_QUIESCED,
                "called synchronize() inside a critical section, {this_seq:#x}",
            );
        }
        // Update the domain's sequence number.
        let seq = self.data.seq.fetch_add(SEQ_INCREMENT, SeqCst) + SEQ_INCREMENT;
        // We need to make sure all threads are quiesced, not busy, or have
        // observed the new sequence number. To do this, we must synchronize the
        // global sequence number update with changes to each thread's local
        // sequence number. To do that, we will issue a membarrier, to broadcast
        // a memory barrier to all threads in the process.
        //
        // First, try to avoid the membarrier if possible--if all threads are quiesced,
        // then there is no need to issue a membarrier, because quiesced threads will issue
        // a memory barrier when they leave the quiesced state.
        if self
            .data
            .threads
            .lock()
            .iter_mut()
            .all(|t| Self::is_thread_ready(t, seq, false))
        {
            return None;
        }
        // Keep a count for diagnostics purposes.
        self.data.membarriers.fetch_add(1, Relaxed);
        sys::membarrier();
        Some(seq)
    }

    /// Synchronizes the RCU domain, blocking asynchronously until all threads
    /// have exited their critical sections and observed the new sequence
    /// number.
    ///
    /// `sleep` should be a function that sleeps for the specified duration.
    pub async fn synchronize(self, mut sleep: impl AsyncFnMut(Duration)) {
        let Some(seq) = self.prepare_to_wait() else {
            return;
        };
        let mut wait = pin!(self.wait_threads_ready(seq));
        let mut timeout = Duration::from_millis(100);
        loop {
            let mut sleep = pin!(sleep(timeout));
            let ready = poll_fn(|cx| {
                if let Poll::Ready(()) = wait.as_mut().poll(cx) {
                    Poll::Ready(true)
                } else if let Poll::Ready(()) = sleep.as_mut().poll(cx) {
                    Poll::Ready(false)
                } else {
                    Poll::Pending
                }
            })
            .await;
            if ready {
                break;
            }
            self.warn_stall(seq);
            if timeout < Duration::from_secs(10) {
                timeout *= 2;
            }
        }
    }

    /// Like [`synchronize`](Self::synchronize), but blocks the current thread
    /// synchronously.
    #[track_caller]
    pub fn synchronize_blocking(self) {
        let Some(seq) = self.prepare_to_wait() else {
            return;
        };
        let mut timeout = Duration::from_millis(10);
        while !self.wait_threads_ready_sync(seq, Instant::now() + timeout) {
            self.warn_stall(seq);
            if timeout < Duration::from_secs(10) {
                timeout *= 2;
            }
        }
    }

    fn warn_stall(&self, target: u64) {
        for thread in &mut *self.data.threads.lock() {
            if !Self::is_thread_ready(thread, target, true) {
                tracelimit::warn_ratelimited!(thread = thread.thread.name(), "rcu stall");
            }
        }
    }

    async fn wait_threads_ready(&self, target: u64) {
        loop {
            let event = self.data.event.listen();
            if self.all_threads_ready(target, true) {
                break;
            }
            event.await;
        }
    }

    #[must_use]
    fn wait_threads_ready_sync(&self, target: u64, deadline: Instant) -> bool {
        loop {
            let event = self.data.event.listen();
            if self.all_threads_ready(target, true) {
                break;
            }
            if event.wait_deadline(deadline).is_none() {
                return false;
            }
        }
        true
    }

    fn all_threads_ready(&self, target: u64, issued_barrier: bool) -> bool {
        self.data
            .threads
            .lock()
            .iter_mut()
            .all(|thread| Self::is_thread_ready(thread, target, issued_barrier))
    }

    fn is_thread_ready(thread: &mut ThreadEntry, target: u64, issued_barrier: bool) -> bool {
        if thread.observed_seq >= target {
            return true;
        }
        let seq = thread.seq_ptr.load(Relaxed);
        assert_ne!(seq, SEQ_NONE);
        if seq & !SEQ_MASK_BUSY < target {
            if seq & SEQ_MASK_BUSY != 0 {
                // The thread is actively running in a critical section.
                return false;
            }
            if seq != SEQ_QUIESCED {
                // The thread is not quiesced. If a barrier was issued, then it
                // has observed the new sequence number. It may be busy (but
                // this CPU has not observed the write yet), but it must be busy
                // with a newer sequence number.
                //
                // If a barrier was not issued, then it is possible that the
                // thread is busy with an older sequence number. In this case,
                // we will need to issue a membarrier to observe the value of
                // the busy bit accurately.
                assert!(seq >= SEQ_FIRST, "{seq}");
                if !issued_barrier {
                    return false;
                }
            }
        }
        thread.observed_seq = target;
        true
    }
}

impl ThreadData {
    #[must_use]
    fn enter(&self, data: &'static RcuData) -> impl '_ + Drop {
        struct ExitOnDrop<'a>(&'a ThreadData, &'static RcuData, u64);
        impl<'a> Drop for ExitOnDrop<'a> {
            fn drop(&mut self) {
                self.0.exit(self.1, self.2);
            }
        }

        // Mark the thread as busy.
        let seq = self.current_seq.load(Relaxed);
        self.current_seq.store(seq | SEQ_MASK_BUSY, Relaxed);
        if seq < SEQ_FIRST {
            self.enter_slow(data, seq);
        }
        // Ensure the busy bit set is visible to the write side before
        // operating on any RCU data.
        sys::access_fence(SeqCst);
        ExitOnDrop(self, data, seq)
    }

    #[cold]
    fn enter_slow(&self, data: &'static RcuData, seq: u64) {
        // The thread was quiesced or not registered. Register it now.
        if seq == SEQ_NONE {
            self.start(data, seq);
        } else {
            debug_assert!(seq == SEQ_QUIESCED || seq & SEQ_MASK_BUSY != 0, "{seq:#x}");
        }
        // Use a full memory barrier to ensure the write side observes that
        // the thread is no longer quiesced before calling `f`.
        fence(SeqCst);
    }

    fn exit(&self, data: &'static RcuData, seq: u64) {
        sys::access_fence(Release);
        // Clear the busy bit.
        self.current_seq.store(seq, Relaxed);
        // Ensure the busy bit clear is visible to the write side, then read the
        // new sequence number, to synchronize with the sequence update path.
        sys::access_fence(SeqCst);
        let new_seq = data.seq.load(Relaxed);
        if new_seq != seq {
            // The domain's current sequence number has changed. Update it and
            // wake up any waiters.
            self.update_seq(data, seq, new_seq);
        }
    }

    #[cold]
    fn start(&self, data: &'static RcuData, seq: u64) {
        if seq == SEQ_NONE {
            // Add the thread to the list of known threads in this domain.
            assert!(self.data.get().is_none());
            data.threads.lock().push(ThreadEntry {
                seq_ptr: TlsRef(&self.current_seq),
                observed_seq: SEQ_NONE,
                thread: std::thread::current(),
            });
            // Remember the domain so that we can remove the thread from the list
            // when it exits.
            self.data.set(Some(data));
        }
    }

    #[cold]
    fn update_seq(&self, data: &'static RcuData, seq: u64, new_seq: u64) {
        if seq & SEQ_MASK_BUSY != 0 {
            // Nested call. Skip.
            return;
        }
        assert!(
            new_seq >= SEQ_FIRST && new_seq & SEQ_MASK_BUSY == 0,
            "{new_seq}"
        );
        self.current_seq.store(new_seq, Relaxed);
        // Wake up any waiters. We don't know how many threads are still in a
        // critical section, so just wake up the writers every time and let them
        // figure it out.
        data.event.notify(!0usize);
    }

    fn quiesce(&self, data: &'static RcuData) -> u64 {
        let seq = self.current_seq.load(Relaxed);
        if seq >= SEQ_FIRST && seq & SEQ_MASK_BUSY == 0 {
            self.current_seq.store(SEQ_QUIESCED, Relaxed);
            data.event.notify(!0usize);
            SEQ_QUIESCED
        } else {
            seq
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::RcuDomain;
    use pal_async::DefaultDriver;
    use pal_async::DefaultPool;
    use pal_async::async_test;
    use pal_async::task::Spawn;
    use pal_async::timer::PolledTimer;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;

    async fn sync(driver: &DefaultDriver, rcu: RcuDomain) {
        let mut timer = PolledTimer::new(driver);
        rcu.synchronize(async |timeout| {
            timer.sleep(timeout).await;
        })
        .await
    }

    #[async_test]
    async fn test_rcu_single(driver: DefaultDriver) {
        define_rcu_domain!(test_rcu);

        test_rcu().run(|| {});
        sync(&driver, test_rcu()).await;
    }

    #[async_test]
    async fn test_rcu_nested(driver: DefaultDriver) {
        define_rcu_domain!(test_rcu);

        test_rcu().run(|| {
            test_rcu().run(|| {});
        });
        sync(&driver, test_rcu()).await;
    }

    #[async_test]
    async fn test_rcu_multi(driver: DefaultDriver) {
        define_rcu_domain!(test_rcu);

        let (thread, thread_driver) = DefaultPool::spawn_on_thread("test");
        thread_driver
            .spawn("test", async { test_rcu().run(|| {}) })
            .await;

        assert_eq!(test_rcu().data.membarriers.load(Ordering::Relaxed), 0);
        sync(&driver, test_rcu()).await;
        assert_eq!(test_rcu().data.membarriers.load(Ordering::Relaxed), 1);

        drop(thread_driver);
        thread.join().unwrap();
    }

    #[async_test]
    async fn test_rcu_multi_quiesce(driver: DefaultDriver) {
        define_rcu_domain!(test_rcu);

        let (thread, thread_driver) = DefaultPool::spawn_on_thread("test");
        thread_driver
            .spawn(
                "test",
                test_rcu().quiesce_on_pending(async { test_rcu().run(|| {}) }),
            )
            .await;

        assert_eq!(test_rcu().data.membarriers.load(Ordering::Relaxed), 0);
        test_rcu().quiesce();
        sync(&driver, test_rcu()).await;
        assert_eq!(test_rcu().data.membarriers.load(Ordering::Relaxed), 0);

        drop(thread_driver);
        thread.join().unwrap();
    }
}
