// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Standalone io-uring integration for the epoll executor.
//!
//! This provides an [`IoUringSubmit`] implementation backed by a lazily
//! created io-uring, with completions driven by the epoll event loop.
//!
//! SQEs are never submitted immediately. Instead, they are queued and
//! flushed in a single `io_uring_enter()` right before `epoll_wait`,
//! giving natural batching of IOs issued in the same poll cycle.
//!
//! Two-tier submit path:
//! 1. On-thread: push directly into the SQ (no lock). If the SQ is
//!    full, flush it with `io_uring_enter()` first.
//! 2. Off-thread: push into a Mutex-protected remote queue, then signal
//!    the wake event to break the epoll thread out of epoll_wait.
//!
//! Completion state is embedded in each [`IoFuture`] (intrusive). The
//! io-uring `user_data` is a pointer to the pinned future's completion
//! state, so no slab or index tracking is needed.

// UNSAFETY: interacts with the io-uring kernel interface, raw pointers for
// intrusive completion tracking, and Pin projections.
#![expect(unsafe_code)]

use crate::io_uring::IoUringSubmit;
use crate::waker::WakerList;
use io_uring::IoUring;
use io_uring::squeue;
use loan_cell::LoanCell;
use parking_lot::Mutex;
use std::cell::Cell;
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::marker::PhantomPinned;
use std::os::unix::io::AsRawFd;
use std::pin::Pin;
use std::process::abort;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

/// Sentinel epoll user data value for the io-uring completion fd.
pub(crate) const EPOLL_URING_TOKEN: u64 = 1;

/// The default submission queue size for the standalone ring.
const DEFAULT_RING_SIZE: u32 = 64;

/// Thread-local state for on-thread io-uring access.
///
/// Lent by the epoll event loop, borrowed by `queue_sqe` to determine
/// whether we are on the epoll thread.
pub(crate) struct EpollUringThreadState {
    /// Pointer to the `EpollIoUring`. Valid for the duration of the loan.
    /// Starts null and is set once the ring is lazily initialized.
    ring: Cell<*const EpollIoUring>,
}

impl EpollUringThreadState {
    pub(crate) fn new(ring: *const EpollIoUring) -> Self {
        Self {
            ring: Cell::new(ring),
        }
    }

    pub(crate) fn set_ring(&self, ring: &EpollIoUring) {
        self.ring.set(ring);
    }
}

// SAFETY: The raw pointer is only dereferenced on the thread that owns the
// loan, which is the same thread that created the `EpollIoUring`.
unsafe impl Send for EpollUringThreadState {}
// SAFETY: Interior mutability is provided by Cell, which is only
// accessed on the owning thread via the LoanCell loan.
unsafe impl Sync for EpollUringThreadState {}

thread_local! {
    pub(crate) static EPOLL_URING_STATE: LoanCell<EpollUringThreadState> =
        const { LoanCell::new() };
}

/// A standalone io-uring that integrates with an epoll event loop.
///
/// Completions are processed by calling [`process_completions`](Self::process_completions)
/// from the epoll event loop when the ring's fd becomes readable.
pub(crate) struct EpollIoUring {
    /// Remote submission queue for off-thread callers. Entries already have
    /// `user_data` set to point at the pinned `IoFuture`'s completion state.
    remote_queue: Mutex<VecDeque<squeue::Entry>>,
    /// Set by off-thread callers when they push to `remote_queue`. Checked
    /// (and cleared) by `flush` to avoid locking the remote queue when
    /// it is empty.
    has_remote: AtomicBool,
    /// The io-uring. The SQ and CQ are only accessed on the epoll thread
    /// via `submission_shared`/`completion_shared` (which use interior
    /// mutability). `probe()` and `submit()` use the submitter, which is
    /// a thread-safe syscall wrapper.
    ring: IoUring,
    /// Wake event fd to signal the epoll thread. Set during creation.
    wake_fd: i32,
}

// SAFETY: `IoUring` is not `Sync` because its SQ/CQ are not thread-safe.
// We guarantee that SQ/CQ access (via `submission_shared`/`completion_shared`)
// only happens on the epoll thread. Off-thread callers only touch
// `remote_queue` (Mutex), `has_remote` (AtomicBool), and `submitter()`
// (thread-safe syscall). `probe()` uses `submitter().register_probe()`,
// which is a thread-safe `io_uring_register` syscall.
unsafe impl Sync for EpollIoUring {}

impl std::fmt::Debug for EpollIoUring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpollIoUring").finish()
    }
}

impl EpollIoUring {
    /// Creates a new standalone ring and registers its fd with the given
    /// epoll fd. `wake_fd` is the raw fd of the wake event, used to signal
    /// the epoll thread from off-thread callers.
    pub(crate) fn new(epoll_fd: i32, wake_fd: i32) -> io::Result<Self> {
        let ring = IoUring::builder().build(DEFAULT_RING_SIZE)?;

        // Register the ring fd with epoll for edge-triggered readable events.
        let mut event = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLET) as u32,
            u64: EPOLL_URING_TOKEN,
        };
        // SAFETY: valid epoll and ring fds.
        let ret =
            unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, ring.as_raw_fd(), &mut event) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            remote_queue: Mutex::new(VecDeque::new()),
            has_remote: AtomicBool::new(false),
            ring,
            wake_fd,
        })
    }

    /// Processes all available completions, waking the associated futures.
    ///
    /// Called from the epoll event loop when the ring fd is readable.
    ///
    /// Loops to handle CQ overflow: when more CQEs are produced than
    /// the CQ can hold, the kernel keeps overflow entries internally.
    /// After draining the visible CQ, an `io_uring_enter()` (via
    /// `submit()`) flushes overflows into the CQ so they can be
    /// drained on the next iteration.
    pub(crate) fn process_completions(&self, wakers: &mut WakerList) {
        loop {
            let mut found = false;
            // SAFETY: we are the sole consumer of the completion queue. The
            // epoll event loop is single-threaded and only calls this method
            // in response to the ring fd being readable.
            while let Some(cqe) = unsafe { self.ring.completion_shared().next() } {
                found = true;
                let ptr = cqe.user_data() as *const Mutex<CompletionState>;
                let result = cqe.result();
                // SAFETY: The pointer is valid because IoFuture aborts on
                // drop if the IO is in flight, guaranteeing the
                // CompletionState outlives the IO.
                let completion = unsafe { &*ptr };
                let CompletionState::Waiting(waker) =
                    std::mem::replace(&mut *completion.lock(), CompletionState::Complete(result))
                else {
                    panic!("already completed");
                };
                wakers.push(waker);
            }
            if !found {
                break;
            }
            // Only issue a syscall when the kernel is holding overflow
            // entries. The IORING_SQ_CQ_OVERFLOW flag is set in the
            // SQ flags when CQEs couldn't fit in the CQ and are queued
            // internally. io_uring_enter() flushes them into the CQ.
            //
            // SAFETY: on the epoll thread — sole SQ accessor.
            if !unsafe { self.ring.submission_shared().cq_overflow() } {
                break;
            }
            let _ = self.ring.submit();
        }
    }

    /// Flushes all queued SQEs into the submission queue and submits them.
    ///
    /// Called from the epoll event loop right before `epoll_wait`. This
    /// drains the remote queue, pushes entries into the SQ, and calls
    /// `io_uring_enter()` once.
    pub(crate) fn flush(&self) {
        // Drain the remote queue, but only take the lock if an
        // off-thread caller has signaled that entries are available.
        if self.has_remote.load(Ordering::Relaxed) {
            let mut remote = self.remote_queue.lock();
            // Clear under the lock — the Mutex provides the necessary
            // ordering, so a relaxed store suffices.
            self.has_remote.store(false, Ordering::Relaxed);
            while let Some(sqe) = remote.pop_front() {
                // SAFETY: only called on the epoll thread, sole SQ accessor.
                unsafe {
                    if self.ring.submission_shared().push(&sqe).is_err() {
                        self.ring.submit().expect("io_uring submit failed");
                        self.ring
                            .submission_shared()
                            .push(&sqe)
                            .expect("SQ still full after submit");
                    }
                }
            }
        }

        // Submit all SQ entries — from the fast path or remote queue.
        //
        // SAFETY: only called on the epoll thread, sole SQ accessor.
        if unsafe { !self.ring.submission_shared().is_empty() } {
            self.ring.submit().expect("io_uring submit failed");
        }
    }

    /// Queue an SQE for deferred submission. The SQE's `user_data` must
    /// already be set to a valid `*const Mutex<CompletionState>`.
    ///
    /// On-thread: tries to push directly into the SQ (fast path), falling
    /// back to the thread-local overflow queue. No syscall.
    ///
    /// Off-thread: pushes into the remote queue and signals the wake event
    /// to break the epoll thread out of `epoll_wait`.
    fn queue_sqe(&self, sqe: &squeue::Entry) {
        EPOLL_URING_STATE.with(|cell| {
            cell.borrow(|state| {
                self.queue_sqe_with_thread_state(state, sqe);
            })
        })
    }

    fn queue_sqe_with_thread_state(
        &self,
        state: Option<&EpollUringThreadState>,
        sqe: &squeue::Entry,
    ) {
        // On-thread: check if this is our ring.
        if let Some(state) = state
            && std::ptr::eq(state.ring.get(), self)
        {
            // Push directly into the SQ.
            // SAFETY: On the epoll thread — sole SQ accessor.
            unsafe {
                if self.ring.submission_shared().push(sqe).is_err() {
                    // SQ full — flush it and retry.
                    self.ring.submit().expect("io_uring submit failed");
                    self.ring
                        .submission_shared()
                        .push(sqe)
                        .expect("SQ still full after submit");
                }
            }
            return;
        }

        // Off-thread (or different ring): use remote queue + wake.
        // The Mutex provides ordering for the queue data; the
        // atomic is just a hint to avoid locking on the consumer
        // side when the queue is empty.
        let needs_wake = {
            let mut remote = self.remote_queue.lock();
            remote.push_back(sqe.clone());
            self.has_remote.store(true, Ordering::Relaxed);
            remote.len() == 1
        };
        if needs_wake {
            // Signal the epoll thread to flush.
            //
            // SAFETY: writing 1u64 to an eventfd is safe and signals it.
            unsafe {
                let val: u64 = 1;
                let r = libc::write(self.wake_fd, std::ptr::from_ref(&val).cast(), 8);
                if r != 8 {
                    panic!("eventfd write failed: {}", io::Error::last_os_error());
                }
            }
        }
    }

    fn probe(&self, opcode: u8) -> bool {
        let mut probe = io_uring::Probe::new();
        self.ring.submitter().register_probe(&mut probe).unwrap();
        probe.is_supported(opcode)
    }
}

impl IoUringSubmit for EpollIoUring {
    fn probe(&self, opcode: u8) -> bool {
        EpollIoUring::probe(self, opcode)
    }

    unsafe fn submit(
        &self,
        sqe: squeue::Entry,
    ) -> Pin<Box<dyn Future<Output = io::Result<i32>> + Send + '_>> {
        Box::pin(IoFuture {
            state: IoFutureState::Init { ring: self, sqe },
        })
    }
}

/// Future returned by [`EpollIoUring::submit`].
///
/// The completion state is embedded in this future, and the io-uring
/// `user_data` is set to the address of the `completion` field. This
/// requires the future to be pinned (ensured by `Box::pin` in `submit`).
///
/// **Aborts on drop** if the IO is in flight, because the kernel holds
/// a pointer into this future's memory.
struct IoFuture<'a> {
    state: IoFutureState<'a>,
}

enum IoFutureState<'a> {
    Init {
        ring: &'a EpollIoUring,
        sqe: squeue::Entry,
    },
    Submitted {
        completion: Mutex<CompletionState>,
        done: bool,
        /// Prevent unpinning.
        _pin: PhantomPinned,
    },
}

/// Per-IO completion state, embedded in each [`IoFuture`].
///
/// The io-uring `user_data` is set to the address of this struct (inside
/// the pinned `IoFuture`). `process_completions` dereferences it to
/// deliver the result.
enum CompletionState {
    Waiting(Waker),
    Complete(i32),
}

// SAFETY: All fields are Send. The Mutex<CompletionState> is shared with
// the completion path but is itself Send+Sync.
unsafe impl Send for IoFuture<'_> {}

impl Future for IoFuture<'_> {
    type Output = io::Result<i32>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<i32>> {
        // SAFETY: we never move out of the pinned future. We only access
        // fields through shared references or take from Option.
        let this = unsafe { self.get_unchecked_mut() };

        match this.state {
            IoFutureState::Init { .. } => {
                let IoFutureState::Init { ring, mut sqe } = std::mem::replace(
                    &mut this.state,
                    IoFutureState::Submitted {
                        completion: Mutex::new(CompletionState::Waiting(cx.waker().clone())),
                        done: false,
                        _pin: PhantomPinned,
                    },
                ) else {
                    unreachable!()
                };
                let IoFutureState::Submitted { completion, .. } = &this.state else {
                    unreachable!()
                };
                // First poll: set user_data to point at our completion state
                // and queue the SQE.
                let completion: &Mutex<CompletionState> = completion;
                let completion_ptr = std::ptr::from_ref(completion);
                sqe.set_user_data(completion_ptr as u64);
                ring.queue_sqe(&sqe);
                Poll::Pending
            }
            IoFutureState::Submitted {
                ref completion,
                ref mut done,
                ..
            } => {
                // Subsequent polls: check for completion.
                let mut state = completion.lock();
                match *state {
                    CompletionState::Waiting(ref mut waker) => waker.clone_from(cx.waker()),
                    CompletionState::Complete(result) => {
                        *done = true;
                        return Poll::Ready(if result >= 0 {
                            Ok(result)
                        } else {
                            Err(io::Error::from_raw_os_error(-result))
                        });
                    }
                }
                Poll::Pending
            }
        }
    }
}

impl Drop for IoFuture<'_> {
    fn drop(&mut self) {
        // If sqe is None, the IO was submitted. Check if it completed.
        match self.state {
            IoFutureState::Init { .. } | IoFutureState::Submitted { done: true, .. } => {}
            IoFutureState::Submitted {
                done: false,
                ref completion,
                ..
            } => {
                match *completion.lock() {
                    CompletionState::Waiting(_) => {
                        // IO is in flight. The kernel holds a pointer to our
                        // completion state. We cannot free this memory.
                        eprintln!("io_uring future dropped with IO in flight, aborting");
                        abort();
                    }
                    CompletionState::Complete(_) => {}
                }
            }
        }
    }
}
