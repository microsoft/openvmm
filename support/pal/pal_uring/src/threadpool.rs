// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements the following functionality:
//! - A threadpool that executes tasks on a pool of affinitized worker threads, and manages a pool of
//!   `IO-Uring`s used to execute asynchronous I/Os. Clients of the threadpool can initiate I/Os on any
//!   of the rings, but each worker thread owns one `IO-Uring` instance and processes all completions
//!   for that instance.
//! - An async task executor that starts a task on the current thread, and then polls it to completion
//!   on an a worker affinitized to the same processor where the task started. The use case for this
//!   executor is the VSCL, where we intend to start an I/O processing task on the VP run thread that
//!   handles a VMBUS interrupt from VTL0, and then later process the I/O completion on the same processor
//!   (either on an affinitized worker thread, or possibly on the VP run thread itself).
//! - A future that represents an async I/O request issued via the IO-Uring mechanism.

use super::ioring::IoCompletionRing;
use super::ioring::IoMemory;
use super::ioring::IoRing;
use futures::FutureExt;
use inspect::Inspect;
use io_uring::opcode;
use io_uring::squeue;
use loan_cell::LoanCell;
use pal_async::task::Runnable;
use pal_async::task::Schedule;
use pal_async::task::Scheduler;
use pal_async::task::Spawn;
use pal_async::task::TaskMetadata;
use pal_async::task::TaskQueue;
use std::borrow::Borrow;
use std::cell::Cell;
use std::cell::RefCell;
use std::fmt::Debug;
use std::future::Future;
use std::future::poll_fn;
use std::io;
use std::os::unix::prelude::*;
use std::pin::Pin;
use std::pin::pin;
use std::process::abort;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Wake;
use std::task::Waker;

/// An io-uring backed pool of tasks and IO.
pub struct IoUringPool {
    client: PoolClient,
    worker: Arc<Worker>,
    completion_ring: IoCompletionRing,
    queue: TaskQueue,
}

impl IoUringPool {
    /// Builds a new pool with the given ring size. `name` is used as the name of the executor.
    pub fn new(name: impl Into<Arc<str>>, ring_size: u32) -> io::Result<Self> {
        let (queue, scheduler) = pal_async::task::task_queue(name);

        let (io_ring, completion_ring) = IoRing::new(ring_size)?;
        let worker = Arc::new(Worker::new(ring_size, io_ring));

        Ok(Self {
            client: PoolClient(worker.clone().initiator(scheduler)),
            worker,
            completion_ring,
            queue,
        })
    }

    /// Returns the client used to configure the pool and get the initiator.
    pub fn client(&self) -> &PoolClient {
        &self.client
    }

    /// Runs the pool until all clients have been dropped and any registered idle
    /// tasks have completed.
    pub fn run(mut self) {
        drop(self.client);
        self.worker.run(self.completion_ring, self.queue.run())
    }
}

/// A client for manipulating a running [`IoUringPool`].
#[derive(Debug, Clone, Inspect)]
#[inspect(transparent)]
pub struct PoolClient(#[inspect(with = "|x| &x.client")] IoInitiator);

impl PoolClient {
    /// Sets the idle task to run. The task is returned by `f`, which receives
    /// the file descriptor of the IO ring.
    ///
    /// The idle task is run before waiting on the IO ring. The idle task can
    /// block synchronously by first calling [`IdleControl::pre_block`], and
    /// then by polling on the IO ring while the task blocks.
    //
    // TODO: move this functionality into underhill_threadpool.
    pub fn set_idle_task<F>(&self, f: F)
    where
        F: 'static + Send + AsyncFnOnce(IdleControl),
    {
        // Keep the pool alive as long as the idle task is running by keeping a
        // clone of this client.
        let keep_pool_alive = self.clone();
        let f = Box::new(|fd| {
            Box::pin(async move {
                let _keep_pool_alive = keep_pool_alive;
                f(fd).await
            }) as Pin<Box<dyn Future<Output = _>>>
        })
            as Box<dyn Send + FnOnce(IdleControl) -> Pin<Box<dyn Future<Output = ()>>>>;

        // Spawn a short-lived task to update the idle task.
        let worker_id = Arc::as_ptr(&self.0.client.worker) as usize; // cast because pointers are not Send
        let task = self.0.spawn("set_idle_task", async move {
            THREADPOOL_WORKER_STATE.with(|state| {
                state.borrow(|state| {
                    let state = state.unwrap();
                    assert_eq!(Arc::as_ptr(&state.worker), worker_id as *const _);
                    state.new_idle_task.set(Some(f));
                })
            })
        });

        task.detach();
    }

    /// Returns the IO initiator.
    pub fn initiator(&self) -> &IoInitiator {
        &self.0
    }

    /// Sets the CPU affinity for the kernel io-uring worker threads.
    pub fn set_iowq_affinity(&self, affinity: &pal::unix::affinity::CpuSet) -> io::Result<()> {
        self.0.client.worker.io_ring.set_iowq_affinity(affinity)
    }

    /// Sets the maximum bounded and unbounded workers (per NUMA node) for the
    /// ring.
    pub fn set_iowq_max_workers(
        &self,
        bounded: Option<u32>,
        unbounded: Option<u32>,
    ) -> io::Result<()> {
        self.0
            .client
            .worker
            .io_ring
            .set_iowq_max_workers(bounded, unbounded)
    }
}

impl Schedule for PoolClient {
    fn schedule(&self, runnable: Runnable) {
        self.0.client.schedule(runnable)
    }

    fn name(&self) -> Arc<str> {
        self.0.client.name()
    }
}

#[derive(Debug, inspect::Inspect)]
pub(crate) struct Worker {
    io_ring_size: u32,
    io_ring: IoRing,
}

type IdleTask = Pin<Box<dyn Future<Output = ()>>>;
type IdleTaskSpawn = Box<dyn Send + FnOnce(IdleControl) -> IdleTask>;

struct AffinitizedWorkerState {
    worker: Arc<Worker>,
    wake: Cell<bool>,
    new_idle_task: Cell<Option<IdleTaskSpawn>>,
    completion_ring: RefCell<IoCompletionRing>,
}

thread_local! {
    static THREADPOOL_WORKER_STATE: LoanCell<AffinitizedWorkerState> = const { LoanCell::new() };
}

impl Wake for Worker {
    fn wake_by_ref(self: &Arc<Self>) {
        THREADPOOL_WORKER_STATE.with(|state| {
            state.borrow(|state| {
                if let Some(state) = state {
                    if Arc::ptr_eq(self, &state.worker) {
                        state.wake.set(true);
                        return;
                    }
                }
                // Submit a nop request to wake up the worker.
                //
                // SAFETY: nop opcode does not reference any data.
                unsafe {
                    self.io_ring.push(opcode::Nop::new().build(), true);
                }
            })
        })
    }

    fn wake(self: Arc<Self>) {
        self.wake_by_ref()
    }
}

impl Worker {
    pub fn new(io_ring_size: u32, io_ring: IoRing) -> Self {
        Self {
            io_ring_size,
            io_ring,
        }
    }

    pub fn initiator(self: Arc<Self>, scheduler: Scheduler) -> IoInitiator {
        IoInitiator {
            client: Arc::new(WorkerClient {
                scheduler,
                worker: self,
            }),
        }
    }

    pub fn run<Fut: Future>(
        self: Arc<Self>,
        completion_ring: IoCompletionRing,
        fut: Fut,
    ) -> Fut::Output {
        tracing::debug!(
            io_ring_size = self.io_ring_size,
            "AffinitizedWorker running"
        );

        let waker = self.clone().into();
        let mut cx = Context::from_waker(&waker);
        let mut fut = pin!(fut);
        let mut idle_task = None;
        let state = AffinitizedWorkerState {
            worker: self,
            wake: Cell::new(false),
            new_idle_task: Cell::new(None),
            completion_ring: RefCell::new(completion_ring),
        };

        THREADPOOL_WORKER_STATE.with(|slot| {
            slot.lend(&state, || {
                loop {
                    // Wake tasks due to IO completion.
                    state.completion_ring.borrow_mut().process();

                    match fut.poll_unpin(&mut cx) {
                        Poll::Ready(r) => {
                            tracing::debug!("AffinitizedWorker exiting");
                            break r;
                        }
                        Poll::Pending => {}
                    }

                    if !state.wake.take() {
                        if let Some(new_idle_task) = state.new_idle_task.take() {
                            idle_task = Some(new_idle_task(IdleControl {
                                inner: state.worker.clone(),
                            }));
                            tracing::debug!("new idle task");
                        }

                        if let Some(task) = &mut idle_task {
                            match task.poll_unpin(&mut cx) {
                                Poll::Ready(()) => {
                                    tracing::debug!("idle task done");
                                    idle_task = None;
                                }
                                Poll::Pending => {}
                            }

                            if state.wake.take() {
                                continue;
                            }
                        }

                        state.worker.io_ring.submit_and_wait();
                    }
                }
            })
        })
    }
}

/// Control interface used by the idle task.
#[derive(Debug)]
pub struct IdleControl {
    inner: Arc<Worker>,
}

impl IdleControl {
    /// Call before blocking in the idle task.
    ///
    /// Returns true if it is OK to block. Returns false if the idle task should
    /// immediately yield instead of blocking.
    pub fn pre_block(&mut self) -> bool {
        THREADPOOL_WORKER_STATE.with(|state| {
            state.borrow(|state| {
                let state = state.unwrap();
                assert!(Arc::ptr_eq(&state.worker, &self.inner));

                // Issue IOs.
                //
                // FUTURE: get the idle task to do this. This will require an
                // io-uring change to allow other drivers to call submit.
                self.inner.io_ring.submit();
                // If the thread was woken or there are completed IOs, ask the idle
                // task to yield.
                !state.wake.get() && state.completion_ring.borrow().is_empty()
            })
        })
    }

    /// The file descriptor of the IO ring.
    ///
    /// The idle task should poll on this fd while blocking.
    pub fn ring_fd(&self) -> BorrowedFd<'_> {
        self.inner.io_ring.as_fd()
    }
}

impl Spawn for IoInitiator {
    fn scheduler(&self, _metadata: &TaskMetadata) -> Arc<dyn Schedule> {
        self.client.clone()
    }
}

#[derive(Debug, inspect::Inspect)]
struct WorkerClient {
    #[inspect(skip)]
    scheduler: Scheduler,
    #[inspect(flatten)]
    worker: Arc<Worker>,
}

impl Schedule for WorkerClient {
    fn schedule(&self, runnable: Runnable) {
        self.scheduler.schedule(runnable)
    }

    fn name(&self) -> Arc<str> {
        self.scheduler.name()
    }
}

/// Client handle for initiating IOs or spawning tasks on a specific threadpool
/// thread.
#[derive(Debug, Clone)]
pub struct IoInitiator {
    client: Arc<WorkerClient>,
}

impl IoInitiator {
    /// Probes the ring for supporting a given opcode.
    pub fn probe(&self, opcode: u8) -> bool {
        self.client.worker.io_ring.probe(opcode)
    }

    /// Issues an IO described by `f`, referencing IO memory in `io_mem`.
    ///
    /// The submission queue entry for the IO is provided by `f` so that the IO
    /// can reference memory in the `io_mem` object. A reference to `io_mem` is
    /// passed to `f` after it has been pinned in memory so that it will not
    /// move for the lifetime of the IO.
    ///
    /// Once the IO has completed, both the result and the IO memory are
    /// returned.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that `f` returns a submission queue entry that
    /// only references memory of static lifetime or that is part of the
    /// `io_mem` object passed to `f`.
    ///
    /// # Aborts
    ///
    /// The process will abort if the async function is dropped before it
    /// completes. This is because the IO memory is not moved into the heap, and
    /// `drop` cannot synchronously wait for the IO to complete.
    pub async unsafe fn issue_io<T, F>(&self, mut io_mem: T, f: F) -> (io::Result<i32>, T)
    where
        T: 'static + Unpin,
        F: FnOnce(&mut T) -> squeue::Entry,
    {
        // Note that this function is written carefully to minimize the
        // generated future size.

        struct AbortOnDrop;

        impl Drop for AbortOnDrop {
            fn drop(&mut self) {
                eprintln!("io dropped in flight, may reference stack memory, aborting process");
                abort();
            }
        }

        // Abort if this future is dropped while the IO is in flight.
        let abort_on_drop = AbortOnDrop;

        // Initiate and wait for the IO.
        let result = poll_fn({
            enum State<F> {
                NotIssued(F),
                Issued(usize),
                Invalid,
            }

            let mut state = State::NotIssued(f);
            let io_mem = &mut io_mem;
            move |cx: &mut Context<'_>| {
                match std::mem::replace(&mut state, State::Invalid) {
                    State::NotIssued(f) => {
                        // SAFETY: validity of the entry is guaranteed by the caller.
                        state = State::Issued(unsafe {
                            self.submit_io((f)(io_mem), IoMemory::new(()), cx.waker().clone())
                        });

                        // Wait once until the waker is woken.
                        Poll::Pending
                    }
                    State::Issued(idx) => match self.poll_io(cx, idx) {
                        Poll::Ready((result, _)) => Poll::Ready(result),
                        Poll::Pending => {
                            state = State::Issued(idx);
                            Poll::Pending
                        }
                    },
                    State::Invalid => unreachable!(),
                }
            }
        })
        .await;

        // The IO is complete, so io_mem is no longer aliased.
        std::mem::forget(abort_on_drop);

        let result = if result >= 0 {
            Ok(result)
        } else {
            Err(io::Error::from_raw_os_error(-result))
        };

        (result, io_mem)
    }

    /// # Safety
    ///
    /// The caller must guarantee that the given io_mem is compatible with the given sqe.
    unsafe fn submit_io(&self, sqe: squeue::Entry, io_mem: IoMemory, waker: Waker) -> usize {
        // Only submit if the worker is not currently running on this thread--if it is, the
        // IO will be submitted soon.
        let needs_submit = THREADPOOL_WORKER_STATE.with(|state| {
            state.borrow(|state| {
                state.is_none_or(|state| !Arc::ptr_eq(&state.worker, &self.client.worker))
            })
        });

        // SAFETY: caller guarantees sqe and io_mem are compatible.
        unsafe {
            self.client
                .worker
                .io_ring
                .new_io(sqe, io_mem, waker, needs_submit)
        }
    }

    fn poll_io(&self, cx: &mut Context<'_>, idx: usize) -> Poll<(i32, IoMemory)> {
        self.client.worker.io_ring.poll_io(cx, idx)
    }

    fn drop_io(&self, idx: usize) {
        self.client.worker.io_ring.drop_io(idx);
    }
}

/// A future representing an IO request submitted to an `IoRingPool`.
pub struct Io<T, Init: Borrow<IoInitiator> = IoInitiator> {
    initiator: Init,
    state: IoState<T>,
}

enum IoState<T> {
    NotStarted(squeue::Entry, T),
    Started(usize),
    Completed(i32, T),
    Invalid,
}

impl<T, Init: Borrow<IoInitiator>> Debug for Io<T, Init> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad("Io")
    }
}

impl<T: 'static + Send + Sync + Unpin, Init: Borrow<IoInitiator> + Unpin> Io<T, Init> {
    /// Creates a new request that will submit the IO described by the submission queue entry
    /// to the specified initiator.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that the `submission_queue_entry` only references memory
    /// owned by the supplied `io_mem` object.
    pub unsafe fn new(initiator: Init, submission_queue_entry: squeue::Entry, io_mem: T) -> Self {
        Self {
            initiator,
            state: IoState::NotStarted(submission_queue_entry, io_mem),
        }
    }

    /// Returns the initiator used to issue the IO.
    pub fn initiator(&self) -> &Init {
        &self.initiator
    }

    /// Issues an async cancel operation for this IO.
    pub fn cancel(&self) {
        // SAFETY: the AsyncCancel entry does not reference any external memory.
        unsafe {
            self.cancel_inner(|user_data| opcode::AsyncCancel::new(user_data).build());
        }
    }

    /// Issues a timeout remove operation for this IO.
    pub fn cancel_timeout(&self) {
        // SAFETY: the TimeoutRemove entry does not reference any external memory.
        unsafe {
            self.cancel_inner(|user_data| opcode::TimeoutRemove::new(user_data).build());
        }
    }

    /// Issues a poll remove operation for this IO.
    pub fn cancel_poll(&self) {
        // SAFETY: the PollRemove entry does not reference any external memory.
        unsafe {
            self.cancel_inner(|user_data| opcode::PollRemove::new(user_data).build());
        }
    }

    /// # Safety
    ///
    /// Caller must ensure that `f` produces a safe sqe entry.
    unsafe fn cancel_inner(&self, f: impl FnOnce(u64) -> squeue::Entry) {
        let sqe = f(self.user_data().unwrap());
        // SAFETY: guaranteed by caller
        let idx = unsafe {
            self.initiator
                .borrow()
                .submit_io(sqe, IoMemory::new(()), Waker::noop().clone())
        };
        self.initiator.borrow().drop_io(idx);
    }

    /// Retrieves the IO memory.
    ///
    /// Panics if the IO has started and has not yet completed.
    pub fn into_mem(mut self) -> T {
        match std::mem::replace(&mut self.state, IoState::Invalid) {
            IoState::Started(_) => {
                panic!("io is not complete");
            }
            IoState::NotStarted(_, io_mem) | IoState::Completed(_, io_mem) => io_mem,
            IoState::Invalid => unreachable!(),
        }
    }

    /// Returns the `user_data` field used when intiating the IO, or `None` if
    /// the IO has not yet been initiated. This is necessary to support
    /// cancelling IOs.
    pub fn user_data(&self) -> Option<u64> {
        match self.state {
            IoState::Started(idx) => Some(idx as u64),
            IoState::NotStarted(_, _) | IoState::Completed(_, _) | IoState::Invalid => None,
        }
    }
}

impl<T: 'static + Sync + Send + Unpin, Init: Borrow<IoInitiator> + Unpin> Future for Io<T, Init> {
    type Output = io::Result<i32>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = Pin::get_mut(self);
        let result = match std::mem::replace(&mut this.state, IoState::Invalid) {
            IoState::NotStarted(entry, io_mem) => {
                // SAFETY: guaranteed by unsafe Self::new.
                let idx = unsafe {
                    this.initiator.borrow().submit_io(
                        entry,
                        IoMemory::new(io_mem),
                        cx.waker().clone(),
                    )
                };
                this.state = IoState::Started(idx);
                return Poll::Pending;
            }
            IoState::Started(idx) => {
                this.state = IoState::Started(idx);
                let (result, io_mem) = std::task::ready!(this.initiator.borrow().poll_io(cx, idx));
                this.state = IoState::Completed(result, io_mem.downcast());
                result
            }
            IoState::Completed(result, io_mem) => {
                this.state = IoState::Completed(result, io_mem);
                result
            }
            IoState::Invalid => unreachable!(),
        };
        let result = if result >= 0 {
            Ok(result)
        } else {
            Err(io::Error::from_raw_os_error(-result))
        };
        Poll::Ready(result)
    }
}

impl<T, Init: Borrow<IoInitiator>> Drop for Io<T, Init> {
    fn drop(&mut self) {
        if let IoState::Started(idx) = self.state {
            self.initiator.borrow().drop_io(idx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Io;
    use super::IoRing;
    use crate::IoUringPool;
    use crate::uring::tests::SingleThreadPool;
    use futures::executor::block_on;
    use io_uring::opcode;
    use io_uring::types;
    use pal_async::task::Spawn;
    use parking_lot::Mutex;
    use std::future::Future;
    use std::os::unix::prelude::*;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Waker;
    use std::thread;
    use std::time::Duration;
    use std::time::Instant;
    use tempfile::NamedTempFile;
    use test_with_tracing::test;

    const PAGE_SIZE: usize = 4096;

    fn new_test_file() -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.as_file_mut().set_len(1024 * 64).unwrap();
        file
    }

    struct Env {
        rx: std::sync::mpsc::Receiver<()>,
    }

    struct TestCase {
        tp: SingleThreadPool,
        file: NamedTempFile,
        _tx: std::sync::mpsc::Sender<()>,
    }

    impl Drop for Env {
        fn drop(&mut self) {
            while self.rx.recv().is_ok() {}
        }
    }

    fn new_test() -> (Env, TestCase) {
        let file = new_test_file();
        let tp = SingleThreadPool::new().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        (Env { rx }, TestCase { tp, file, _tx: tx })
    }

    struct Timeout {
        shared_state: Arc<Mutex<TimeoutState>>,
    }

    struct TimeoutState {
        completed: bool,
        waker: Option<Waker>,
    }

    impl Future for Timeout {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let mut shared_state = self.shared_state.lock();
            if shared_state.completed {
                Poll::Ready(())
            } else {
                shared_state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }

    impl Timeout {
        fn new(duration: Duration) -> Self {
            let shared_state = Arc::new(Mutex::new(TimeoutState {
                completed: false,
                waker: None,
            }));

            // Spawn the new thread
            let thread_shared_state = shared_state.clone();
            thread::spawn(move || {
                thread::sleep(duration);
                let mut shared_state = thread_shared_state.lock();
                shared_state.completed = true;
                if let Some(waker) = shared_state.waker.take() {
                    waker.wake()
                }
            });

            Timeout { shared_state }
        }
    }

    /// Skips a test case if IO-Uring is not supported (this is necessary because IO-Uring is not yet supported
    /// by the official WSL2 kernel).
    macro_rules! skip_if_no_io_uring_support {
        () => {
            if IoRing::new(1).is_err() {
                println!("Test case skipped (no IO-Uring support)");
                return;
            }
        };
    }

    #[test]
    fn test_task_executor() {
        skip_if_no_io_uring_support!();
        let tp = SingleThreadPool::new().unwrap();

        let (tx, rx) = std::sync::mpsc::channel();

        tp.initiator()
            .spawn("test", async move {
                let now = Instant::now();
                Timeout::new(Duration::from_secs(2)).await;
                assert!(now.elapsed().as_secs() >= 2);
                tx.send(()).unwrap();
            })
            .detach();

        rx.recv().unwrap();
    }

    #[test]
    fn test_local_task_executor() {
        skip_if_no_io_uring_support!();
        let tp = SingleThreadPool::new().unwrap();

        let (tx, rx) = std::sync::mpsc::channel();

        tp.initiator()
            .spawn("test", async move {
                let now = Instant::now();
                Timeout::new(Duration::from_secs(2)).await;
                assert!(now.elapsed().as_secs() >= 2);
                tx.send(()).unwrap();
            })
            .detach();

        rx.recv().unwrap();
    }

    #[test]
    fn test_serial_io() {
        skip_if_no_io_uring_support!();
        let (_env, test) = new_test();

        block_on(async move {
            let _ = &test;
            let mut write_buf = vec![0u8; PAGE_SIZE];
            for (i, b) in write_buf.iter_mut().enumerate() {
                *b = i as u8;
            }

            let sqe = opcode::Write::new(
                types::Fd(test.file.as_fd().as_raw_fd()),
                write_buf.as_ptr(),
                write_buf.len() as _,
            )
            .offset(0)
            .build();

            // SAFETY: The only memory being referenced in the submission is write_buf.
            let mut write_io = unsafe { Io::new(test.tp.initiator(), sqe, write_buf) };
            (&mut write_io).await.unwrap();
            let write_buf = write_io.into_mem();

            let sqe = opcode::Fsync::new(types::Fd(test.file.as_fd().as_raw_fd())).build();
            // SAFETY: the Fsync entry does not reference any external memory.
            unsafe {
                Io::new(test.tp.initiator(), sqe, ()).await.unwrap();
            }

            let mut read_buf = vec![0u8; PAGE_SIZE];
            let sqe = opcode::Read::new(
                types::Fd(test.file.as_fd().as_raw_fd()),
                read_buf.as_mut_ptr(),
                read_buf.len() as _,
            )
            .offset(0)
            .build();

            // SAFETY: The only memory being referenced in the submission is read_buf.
            let mut read_io = unsafe { Io::new(test.tp.initiator(), sqe, read_buf) };
            (&mut read_io).await.unwrap();
            let read_buf = read_io.into_mem();

            assert_eq!(&write_buf[..], &read_buf[..]);
        });
    }

    #[test]
    fn test_stack_io() {
        skip_if_no_io_uring_support!();
        let (_env, test) = new_test();

        block_on(async move {
            let _ = &test;
            let mut write_buf = [0; 100];
            for (i, b) in write_buf.iter_mut().enumerate() {
                *b = i as u8;
            }

            // SAFETY: The only memory being referenced in the submission is write_buf.
            let (r, write_buf) = unsafe {
                test.tp
                    .initiator()
                    .issue_io(write_buf, |write_buf| {
                        opcode::Write::new(
                            types::Fd(test.file.as_fd().as_raw_fd()),
                            write_buf.as_ptr(),
                            write_buf.len() as _,
                        )
                        .offset(0)
                        .build()
                    })
                    .await
            };
            r.unwrap();

            // SAFETY: the Fsync entry does not reference any external memory.
            unsafe {
                test.tp
                    .initiator()
                    .issue_io((), |_| {
                        opcode::Fsync::new(types::Fd(test.file.as_fd().as_raw_fd())).build()
                    })
                    .await
                    .0
                    .unwrap();
            }

            let read_buf = [0u8; 100];
            // SAFETY: the buffer is owned by the IO for its lifetime.
            let (r, read_buf) = unsafe {
                test.tp
                    .initiator()
                    .issue_io(read_buf, |read_buf| {
                        opcode::Read::new(
                            types::Fd(test.file.as_fd().as_raw_fd()),
                            read_buf.as_mut_ptr(),
                            read_buf.len() as _,
                        )
                        .offset(0)
                        .build()
                    })
                    .await
            };
            r.unwrap();

            assert_eq!(&write_buf[..], &read_buf[..]);
        });
    }

    // TODO: This test requires higher memlock limits that scale with processor count, as set in
    //       /etc/security/limits.conf and with ulimit -l.
    //
    //       Disable these in CI for now until the code is more aware of limits and can handle them and/or io-uring no
    //       longer requires locked pages. A 16 core build agent requires more than the default set.
    #[test]
    #[cfg(not(feature = "ci"))]
    fn test_split_io() {
        skip_if_no_io_uring_support!();
        let (_env, test) = new_test();

        block_on(async move {
            let _ = &test;
            let mut write_buf1 = vec![0u8; PAGE_SIZE];
            for (i, b) in write_buf1.iter_mut().enumerate() {
                *b = i as u8;
            }
            let sqe1 = opcode::Write::new(
                types::Fd(test.file.as_fd().as_raw_fd()),
                write_buf1.as_mut_ptr(),
                write_buf1.len() as _,
            )
            .offset(0)
            .build();
            // SAFETY: The only memory being referenced in the submission is write_buf1.
            let write1 = unsafe { Io::new(test.tp.initiator(), sqe1, write_buf1) };

            let mut write_buf2 = vec![0u8; PAGE_SIZE];
            for (i, b) in write_buf2.iter_mut().enumerate() {
                *b = i as u8;
            }
            let sqe2 = opcode::Write::new(
                types::Fd(test.file.as_fd().as_raw_fd()),
                write_buf2.as_mut_ptr(),
                write_buf2.len() as _,
            )
            .offset(4096)
            .build();
            // SAFETY: The only memory being referenced in the submission is write_buf2.
            let write2 = unsafe { Io::new(test.tp.initiator(), sqe2, write_buf2) };

            let (r1, r2) = futures::join!(write1, write2);
            r1.unwrap();
            r2.unwrap();
        });
    }

    // TODO: This test requires higher memlock limits that scale with processor count, as set in
    //       /etc/security/limits.conf and with ulimit -l.
    //
    //       Disable these in CI for now until the code is more aware of limits and can handle them and/or io-uring no
    //       longer requires locked pages. A 16 core build agent requires more than the default set.
    #[test]
    #[cfg(not(feature = "ci"))]
    fn test_tp_io() {
        skip_if_no_io_uring_support!();
        let (_env, test) = new_test();

        test.tp
            .initiator()
            .clone()
            .spawn("test", async move {
                let _ = &test;
                let mut write_buf = vec![0u8; PAGE_SIZE];
                for (i, b) in write_buf.iter_mut().enumerate() {
                    *b = i as u8;
                }

                let sqe = opcode::Write::new(
                    types::Fd(test.file.as_fd().as_raw_fd()),
                    write_buf.as_mut_ptr(),
                    write_buf.len() as _,
                )
                .offset(0)
                .build();

                // SAFETY: The only memory being referenced in the submission is write_buf.
                let mut write_io = unsafe { Io::new(test.tp.initiator(), sqe, write_buf) };
                (&mut write_io).await.unwrap();
                let write_buf = write_io.into_mem();

                let mut read_buf = vec![0u8; PAGE_SIZE];

                let sqe = opcode::Read::new(
                    types::Fd(test.file.as_fd().as_raw_fd()),
                    read_buf.as_mut_ptr(),
                    read_buf.len() as _,
                )
                .offset(0)
                .build();

                // SAFETY: The only memory being referenced in the submission is read_buf.
                let mut read_io = unsafe { Io::new(test.tp.initiator(), sqe, read_buf) };
                (&mut read_io).await.unwrap();
                let read_buf = read_io.into_mem();

                assert_eq!(&write_buf[..], &read_buf[..]);
            })
            .detach();
    }

    #[test]
    fn test_run_until_none() {
        skip_if_no_io_uring_support!();
        let (send, recv) = async_channel::bounded(1);
        let (send2, recv2) = async_channel::bounded(1);
        let pool = IoUringPool::new("test", 16).unwrap();
        let done = Arc::new(AtomicBool::new(false));
        pool.client()
            .initiator()
            .spawn("hmm", {
                async move {
                    recv.recv().await.unwrap();
                    send2.send(()).await.unwrap();
                }
            })
            .detach();
        pool.client().set_idle_task({
            let done = done.clone();
            |_ctl| async move {
                send.send(()).await.unwrap();
                recv2.recv().await.unwrap();
                done.store(true, Ordering::SeqCst);
            }
        });
        pool.run();
        assert!(done.load(Ordering::SeqCst));
    }
}
