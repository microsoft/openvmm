// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Lower-level async io-uring support, not tied to an executor model.

use ::smallbox::SmallBox;
use ::smallbox::space::S4;
use io_uring::IoUring;
use io_uring::squeue;
use pal::unix::SyscallResult;
use pal::unix::affinity::CpuSet;
use pal::unix::while_eintr;
use parking_lot::Mutex;
use slab::Slab;
use smallbox::smallbox;
use std::any::Any;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::io;
use std::os::unix::prelude::*;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

/// An object that owns memory needed for an asynchronous IO.
///
/// This object is passed to the ring to ensure that any pointers referenced by
/// the IO are kept alive for the duration of the IO, even if the issuing task
/// is dropped or forgotten.
///
/// The internals use a SmallBox<_, S4> to avoid allocations for common sizes.
pub struct IoMemory(SmallBox<dyn Any + Sync + Send + Unpin, S4>);

impl IoMemory {
    /// Creates a new memory, erasing the type.
    pub fn new(v: impl 'static + Sync + Send + Unpin) -> Self {
        Self(smallbox!(v))
    }

    /// Converts the memory back to an unerased type.
    ///
    /// Panics if the type is not the same as that of the object passed to
    /// `new`.
    pub fn downcast<T: Any>(self) -> T {
        // Remove the Unpin bound so that `downcast` is available.
        //
        // SAFETY: The vtable for Any is the same as Any + Sync + Send + Unpin.
        let inner: SmallBox<dyn Any, S4> = unsafe { std::mem::transmute(self.0) };
        let inner: SmallBox<T, _> = inner.downcast().unwrap();
        inner.into_inner()
    }
}

impl Debug for IoMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad("IoMemory")
    }
}

/// An IO submission ring.
pub struct IoRing {
    inner: Arc<RingInner>,
}

struct RingInner {
    ring: IoUring,
    state: Mutex<RingState>,
    pending_io_count: AtomicUsize,
}

struct RingState {
    // The list of outstanding IOs. This is locked with a single Mutex because
    // IOs are generally expected to be issued and completed on a single thread,
    // so contention should be rare.
    iocbs: Slab<Iocb>,
    // The submission IO overflow queue.
    //
    // FUTURE: instead of maintaining a queue, consider providing backpressure
    // to initiators.
    queue: VecDeque<QueueEntry>,
}

impl Debug for IoRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IoRing").finish()
    }
}

impl AsFd for IoRing {
    fn as_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: fd is valid as long as `ring`
        unsafe { BorrowedFd::borrow_raw(self.inner.ring.as_raw_fd()) }
    }
}

struct QueueEntry {
    sqe: squeue::Entry,
    iocb_idx: Option<usize>,
}

/// An IO completion ring.
pub struct IoCompletionRing {
    inner: Arc<RingInner>,
    // Keep this Vec around to avoid repeated allocations.
    results: Vec<Result<Waker, Iocb>>,
}

impl IoCompletionRing {
    /// Processes the completion ring, waking any tasks waiting on IO.
    pub fn process(&mut self) {
        // Collect the wakers and dropped IO with the IOCB lock held.
        {
            let mut state = self.inner.state.lock();
            // SAFETY: Callers of `completion_shared` must ensure that no other
            // `CompletionQueue` may exist at the time of the call. This is
            // guaranteed because there is only once instance of
            // IoCompletionRing per io-uring.
            while let Some(cqe) = unsafe { self.inner.ring.completion_shared().next() } {
                let result = cqe.result();

                if cqe.user_data() != !0 {
                    let idx = cqe.user_data() as usize;
                    let iocb = &mut state.iocbs[idx];
                    match std::mem::replace(&mut iocb.state, IoState::Completed(result)) {
                        IoState::Waiting(waker) => {
                            self.results.push(Ok(waker));
                        }
                        IoState::Completed(_) => panic!("io double completed"),
                        IoState::Dropped => {
                            self.results.push(Err(state.iocbs.remove(idx)));
                            self.inner.pending_io_count.fetch_sub(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        }

        // Wake the tasks and drop IOCBs outside the lock.
        for result in self.results.drain(..) {
            if let Ok(waker) = result {
                waker.wake();
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        // SAFETY: there is only one instance of this type per io-uring, so
        // there cannot be other concurrent users of the completion ring.
        unsafe { self.inner.ring.completion_shared().is_empty() }
    }
}

impl IoRing {
    /// Creates a new `IoRing` wrapper and the underlying kernel io-uring.
    ///
    /// # Arguments
    ///
    /// * `size` - The maximum number of entries in the submission queue. The completion queue is
    ///   twice the size of the submission queue. Note that this is not strictly a limit on the maximum
    ///   number of outstanding I/Os, rather it's the maximum number of I/Os that the IoRing client
    ///   can allow to batch (either in the submission or completion paths).
    pub fn new(size: u32) -> Result<(IoRing, IoCompletionRing), io::Error> {
        let inner = Arc::new(RingInner {
            ring: IoUring::builder().build(size)?,
            state: Mutex::new(RingState {
                iocbs: Slab::new(),
                queue: VecDeque::with_capacity(size as usize),
            }),
            pending_io_count: AtomicUsize::new(0),
        });

        let this = IoRing {
            inner: inner.clone(),
        };
        let cring = IoCompletionRing {
            inner,
            results: Vec::new(),
        };
        Ok((this, cring))
    }

    /// Sets the maximum bounded and unbounded workers (per NUMA node) for the
    /// ring.
    pub fn set_iowq_max_workers(
        &self,
        bounded: Option<u32>,
        unbounded: Option<u32>,
    ) -> io::Result<()> {
        self.inner
            .ring
            .submitter()
            .register_iowq_max_workers(&mut [bounded.unwrap_or(0), unbounded.unwrap_or(0)])
    }

    /// io_uring doesn't support IORING_REGISTER_IOWQ_AFF yet, so create an unsafe version.
    /// We should use the io_uring API in the future.
    pub fn set_iowq_affinity(&self, cpu_set: &CpuSet) -> io::Result<()> {
        // SAFETY: calling as documented, with appropriately-sized buffer.
        // According to the doc, IORING_REGISTER_IOWQ_AFF expects nr_args as the
        // byte size of cpu mask.
        unsafe {
            let ret = libc::syscall(
                libc::SYS_io_uring_register,
                self.inner.ring.as_raw_fd(),
                17, // IORING_REGISTER_IOWQ_AFF
                cpu_set.as_ptr(),
                cpu_set.buffer_len(),
            );

            if ret < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn try_submit(&self) {
        if let Err(e) = self.inner.ring.submit() {
            if e.raw_os_error() == Some(libc::EBUSY) {
                tracing::trace!("completion queue is full");
            } else {
                panic!("iouring submit failed: {}", e);
            }
        }
    }

    fn sqe(entry: &QueueEntry) -> squeue::Entry {
        entry
            .sqe
            .clone()
            .user_data(entry.iocb_idx.unwrap_or(!0) as u64)
    }

    // Flushes as many entries as possible from the overflow queue to the submission queue and
    // optionally queues a new entry.
    fn flush_queue(&self, state: &mut RingState, new_entry: Option<QueueEntry>) -> bool {
        assert_eq!(std::ptr::from_mut(state), self.inner.state.data_ptr());
        // SAFETY: Callers of submission_shared must ensure that no other
        // SubmissionQueue may exist at the time of the call. This is guaranteed
        // by holding the lock associated with `state`.
        unsafe {
            let mut ring = self.inner.ring.submission_shared();
            if ring.is_full() {
                tracing::trace!("submission queue is full");
                drop(ring);
                self.try_submit();
                ring = self.inner.ring.submission_shared();
            }

            while let Some(entry) = state.queue.front() {
                if ring.push(&Self::sqe(entry)).is_err() {
                    break;
                }
                state.queue.pop_front();
            }

            if let Some(entry) = new_entry {
                if ring.push(&Self::sqe(&entry)).is_err() {
                    state.queue.push_back(entry);
                }
            }

            !ring.is_empty()
        }
    }

    /// Submits as many entries as possible.
    pub fn submit(&self) {
        // Push entries from the overflow queue.
        if self.flush_queue(&mut self.inner.state.lock(), None) {
            self.try_submit();
        }
    }

    /// Submits as many entries as possible and waits for the next completion.
    pub fn submit_and_wait(&self) {
        // Push entries from the overflow queue.
        self.flush_queue(&mut self.inner.state.lock(), None);

        // Attempt to submit all entries
        while_eintr(|| self.inner.ring.submit_and_wait(1)).unwrap_or_else(|e| {
            assert_eq!(e.raw_os_error(), Some(libc::EBUSY));
            tracing::trace!("completion queue is full");
            // Completion queue is full. Wait on the ring fd without submitting any entries,
            // the caller will consume some completion entries and try to submit again.
            let mut pollfd = libc::pollfd {
                fd: self.inner.ring.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: calling poll with a valid pollfd.
            unsafe {
                while_eintr(|| libc::poll(&mut pollfd, 1, -1).syscall_result()).unwrap();
            };
            0
        });
    }

    /// Pushes a new IO in the ring and optionally submits it. Returns an index
    /// to be used for tracking.
    ///
    /// The IO can be polled for completion with [`Self::poll_io`], or the
    /// result discarded with [`Self::drop_io`].
    ///
    /// # Safety
    ///
    /// The caller must ensure that `sqe` is valid and that `io_mem` will ensure
    /// the lifetime of any required buffers.
    pub unsafe fn new_io(
        &self,
        sqe: squeue::Entry,
        io_mem: IoMemory,
        waker: Waker,
        submit: bool,
    ) -> usize {
        let iocb_idx;
        {
            let mut state = self.inner.state.lock();
            iocb_idx = state.iocbs.insert(Iocb {
                state: IoState::Waiting(waker),
                io_mem,
            });
            self.inner.pending_io_count.fetch_add(1, Ordering::Relaxed);
            self.flush_queue(
                &mut state,
                Some(QueueEntry {
                    sqe,
                    iocb_idx: Some(iocb_idx),
                }),
            );
        }

        if submit {
            self.try_submit();
        }

        iocb_idx
    }

    /// Pushes an IO to the ring and optionally submits it.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `sqe` is valid and does not reference any
    /// external buffers.
    pub unsafe fn push(&self, sqe: squeue::Entry, submit: bool) {
        let entry = QueueEntry {
            sqe,
            iocb_idx: None,
        };
        self.flush_queue(&mut self.inner.state.lock(), Some(entry));
        if submit {
            self.try_submit();
        }
    }

    /// Checks whether the specified opcode is supported by this `IoRing`.
    pub fn probe(&self, opcode: u8) -> bool {
        let mut probe = io_uring::Probe::new();
        self.inner
            .ring
            .submitter()
            .register_probe(&mut probe)
            .unwrap();
        probe.is_supported(opcode)
    }

    /// Polls an IO for completion.
    ///
    /// If the IO is completed, returns the status and associated memory object.
    /// The IO is no longer tracked after this, so do not call `drop_io`.
    pub fn poll_io(&self, cx: &mut Context<'_>, idx: usize) -> Poll<(i32, IoMemory)> {
        let mut state = self.inner.state.lock();
        let iocb = &mut state.iocbs[idx];
        match &mut iocb.state {
            IoState::Waiting(old_waker) => {
                old_waker.clone_from(cx.waker());
            }
            IoState::Completed(status) => {
                let status = *status;
                let iocb = state.iocbs.remove(idx);
                self.inner.pending_io_count.fetch_sub(1, Ordering::Relaxed);
                return Poll::Ready((status, iocb.io_mem));
            }
            IoState::Dropped => {
                panic!("polling dropped io");
            }
        }
        Poll::Pending
    }

    /// Releases an IO without consuming its result.
    ///
    /// This does not cancel the IO. It just directs the completion ring to
    /// release the associated resources after the IO completes, since no task
    /// plans to poll it.
    pub fn drop_io(&self, idx: usize) {
        let mut state = self.inner.state.lock();
        let iocb = &mut state.iocbs[idx];
        match &iocb.state {
            IoState::Waiting(_) => {
                iocb.state = IoState::Dropped;
            }
            IoState::Completed(_) => {
                let iocb = state.iocbs.remove(idx);
                self.inner.pending_io_count.fetch_sub(1, Ordering::Relaxed);
                drop(state);
                drop(iocb);
            }
            IoState::Dropped => {
                panic!("double dropped an io");
            }
        }
    }
}

impl inspect::Inspect for IoRing {
    fn inspect(&self, req: inspect::Request<'_>) {
        let state = self.inner.state.lock();
        let mut completed = 0;
        let mut waiting = 0;
        let mut dropped = 0;
        state.iocbs.iter().for_each(|i| match i.1.state {
            IoState::Waiting(_) => waiting += 1,
            IoState::Completed(_) => completed += 1,
            IoState::Dropped => dropped += 1,
        });
        req.respond()
            .field("iocbs_allocated", state.iocbs.len())
            .field("iocbs_queued", state.queue.len())
            .field("iocbs_waiting", waiting)
            .field("iocbs_completed", completed)
            .field("iocbs_dropped", dropped);
    }
}

#[derive(Debug)]
struct Iocb {
    state: IoState,
    io_mem: IoMemory,
}

/// The completion state of an asynchronous IO.
#[derive(Debug)]
enum IoState {
    Waiting(Waker),
    Completed(i32),
    Dropped,
}
