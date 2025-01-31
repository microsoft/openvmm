// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Task spawning support.

// UNSAFETY: Managing information stored as pointers for debugging purposes.
#![expect(unsafe_code)]

use loan_cell::LoanCell;
use parking_lot::Mutex;
use slab::Slab;
use std::fmt::Debug;
use std::fmt::Display;
use std::future::Future;
use std::panic::Location;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Weak;

/// A handle to a task.
pub type Task<T> = async_task::Task<T, TaskMetadata>;

/// A handle to a task that's ready to run.
pub type Runnable = async_task::Runnable<TaskMetadata>;

/// Metadata about a spawned task.
///
/// This can be accessed via [`Task::metadata()`], [`Runnable::metadata()`], or
/// [`with_current_task_metadata()`].
#[derive(Debug)]
pub struct TaskMetadata {
    name: Arc<str>,
    location: &'static Location<'static>,
    /// The ready/waiting/running/done state of the future itself. Tracked for
    /// diagnostics purposes.
    state: AtomicU32,
    /// Whether the task has been dropped. This could be a high bit in `state`
    /// or something, but keep this separate to make the codegen straightforward
    /// for all state updates.
    dropped: AtomicBool,
    scheduler: Weak<dyn Schedule>,
    id: AtomicUsize,
    _no_pin: std::marker::PhantomPinned,
}

impl TaskMetadata {
    const NO_ID: usize = !0;

    #[track_caller]
    fn new(name: Arc<str>) -> Self {
        Self {
            name,
            location: Location::caller(),
            state: AtomicU32::new(TASK_STATE_READY),
            dropped: AtomicBool::new(false),
            scheduler: Weak::<Scheduler>::new(),
            id: AtomicUsize::new(Self::NO_ID),
            _no_pin: std::marker::PhantomPinned,
        }
    }

    fn register(self: Pin<&Self>) {
        assert_eq!(self.id.load(Ordering::Relaxed), Self::NO_ID);
        // Insert a pointer into the global task list. This is safe because this
        // object is known to be pinned, so its storage will not be deallocated
        // without calling `drop` (which will remove it from the list).
        let id = TASK_LIST
            .slab
            .lock()
            .insert(TaskMetadataPtr(self.get_ref()));
        self.id.store(id, Ordering::Relaxed);
    }

    fn pend(&self) {
        self.state.store(TASK_STATE_WAITING, Ordering::Relaxed);
    }

    fn done(&self) {
        self.state.store(TASK_STATE_DONE, Ordering::Relaxed);
    }

    fn run(&self) {
        self.state.store(TASK_STATE_RUNNING, Ordering::Relaxed);
    }

    /// The name of the spawned task.
    pub fn name(&self) -> &Arc<str> {
        &self.name
    }

    /// The location where the task was spawned.
    pub fn location(&self) -> &'static Location<'static> {
        self.location
    }

    /// The current state of the task.
    fn state(&self) -> TaskState {
        let state = self.state.load(Ordering::Relaxed);
        if self.dropped.load(Ordering::Relaxed) {
            if state == TASK_STATE_DONE {
                TaskState::Complete
            } else {
                TaskState::Cancelled
            }
        } else {
            match self.state.load(Ordering::Relaxed) {
                TASK_STATE_READY => TaskState::Ready,
                TASK_STATE_WAITING => TaskState::Waiting,
                TASK_STATE_RUNNING => TaskState::Running,
                TASK_STATE_DONE => TaskState::Complete,
                _ => unreachable!(),
            }
        }
    }
}

impl Drop for TaskMetadata {
    fn drop(&mut self) {
        let id = self.id.load(Ordering::Relaxed);
        if id != Self::NO_ID {
            let _task = TASK_LIST.slab.lock().remove(id);
        }
    }
}

#[derive(Debug, Copy, Clone)]
struct TaskMetadataPtr(*const TaskMetadata);

// Assert `TaskMetadata` is `Send` and `Sync`.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<TaskMetadata>();
};

// SAFETY: `TaskMetadata` can be safely shared between threads (asserted above).
unsafe impl Send for TaskMetadataPtr {}
// SAFETY: `TaskMetadata` can be safely shared between threads (asserted above).
unsafe impl Sync for TaskMetadataPtr {}

/// A queue of tasks that will be run on a single thread.
#[derive(Debug)]
pub struct TaskQueue {
    tasks: async_channel::Receiver<Runnable>,
}

/// A task scheduler for a [`TaskQueue`].
#[derive(Debug)]
pub struct Scheduler {
    send: async_channel::Sender<Runnable>,
    name: Mutex<Arc<str>>,
}

impl Scheduler {
    /// Updates the name of the scheduler.
    pub fn set_name(&self, name: impl Into<Arc<str>>) {
        *self.name.lock() = name.into();
    }
}

impl Schedule for Scheduler {
    fn schedule(&self, runnable: Runnable) {
        let _ = self.send.try_send(runnable);
    }

    fn name(&self) -> Arc<str> {
        self.name.lock().clone()
    }
}

/// Creates a new task queue and associated scheduler.
pub fn task_queue(name: impl Into<Arc<str>>) -> (TaskQueue, Scheduler) {
    let (send, recv) = async_channel::unbounded();
    (
        TaskQueue { tasks: recv },
        Scheduler {
            send,
            name: Mutex::new(name.into()),
        },
    )
}

impl TaskQueue {
    /// Runs tasks on the queue.
    ///
    /// Returns when the associated scheduler has been dropped.
    pub async fn run(&mut self) {
        while let Ok(task) = self.tasks.recv().await {
            task.run();
        }
    }
}

/// Trait for scheduling a task on an executor.
pub trait Schedule: Send + Sync {
    /// Schedules a task to run.
    fn schedule(&self, runnable: Runnable);

    /// Gets the executor name.
    fn name(&self) -> Arc<str>;
}

struct TaskFuture<'a, Fut> {
    metadata: &'a TaskMetadata,
    _scheduler: Arc<dyn Schedule>, // Keep the scheduler alive until the future is dropped.
    future: Fut,
}

impl<'a, Fut: Future> TaskFuture<'a, Fut> {
    fn new(metadata: Pin<&'a TaskMetadata>, scheduler: Arc<dyn Schedule>, future: Fut) -> Self {
        metadata.register();
        Self {
            metadata: metadata.get_ref(),
            _scheduler: scheduler,
            future,
        }
    }

    /// Wrapper around `new` for passing to [`async_task::Builder::spawn`].
    ///
    /// # Safety
    /// The caller guarantees that the incoming `metadata` pointer is pinned and
    /// that the future will not be used beyond the lifetime of `metadata`
    /// (despite having a static lifetime). This is guaranteed by
    /// [`async_task::Builder::spawn`] API, which unfortunately is missing the
    /// explicit `Pin` and the appropriate lifetime on the future.
    ///
    /// See <https://github.com/smol-rs/async-task/issues/76>.
    unsafe fn new_for_async_task(
        metadata: &'a TaskMetadata,
        scheduler: Arc<dyn Schedule>,
        future: Fut,
    ) -> TaskFuture<'static, Fut> {
        // SAFETY: `metadata` is pinned by `async_task::Builder::spawn`, and the
        // caller guarantees this function will only be used in that context.
        let metadata = unsafe { Pin::new_unchecked(metadata) };
        let this = Self::new(metadata, scheduler, future);
        // Transmute to static lifetime, as required by
        // `async_task::Builder::spawn`.
        //
        // SAFETY: the caller guarantees this future will only be passed to
        // `spawn`, which will guarantee the metadata is not dropped before the
        // future is dropped.
        unsafe { std::mem::transmute::<TaskFuture<'a, Fut>, TaskFuture<'static, Fut>>(this) }
    }
}

impl<Fut: Future> Future for TaskFuture<'_, Fut> {
    type Output = Fut::Output;

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        // SAFETY: projecting this type for pinned access to the future. The
        // future will not be moved or dropped.
        let this = unsafe { self.get_unchecked_mut() };
        this.metadata.run();
        // SAFETY: the future is pinned since `self` is pinned.
        let future = unsafe { Pin::new_unchecked(&mut this.future) };
        let r = CURRENT_TASK.with(|task| task.lend(this.metadata, || future.poll(cx)));
        if r.is_pending() {
            this.metadata.pend();
        } else {
            this.metadata.done();
        }
        r
    }
}

impl<Fut> Drop for TaskFuture<'_, Fut> {
    fn drop(&mut self) {
        self.metadata.dropped.store(true, Ordering::Relaxed);
    }
}

fn schedule(runnable: Runnable) {
    let metadata = runnable.metadata();
    metadata.state.store(TASK_STATE_READY, Ordering::Relaxed);
    if let Some(scheduler) = metadata.scheduler.upgrade() {
        scheduler.schedule(runnable);
    }
}

/// Trait for spawning a task on an executor.
pub trait Spawn: Send + Sync {
    /// Gets a scheduler for a new task.
    fn scheduler(&self, metadata: &TaskMetadata) -> Arc<dyn Schedule>;

    /// Spawns a task.
    #[track_caller]
    fn spawn<T: 'static + Send>(
        &self,
        name: impl Into<Arc<str>>,
        fut: impl Future<Output = T> + Send + 'static,
    ) -> Task<T>
    where
        Self: Sized,
    {
        let mut metadata = TaskMetadata::new(name.into());
        let scheduler = self.scheduler(&metadata);
        metadata.scheduler = Arc::downgrade(&scheduler);
        let (runnable, task) = async_task::Builder::new().metadata(metadata).spawn(
            |metadata| {
                // SAFETY: calling from the async_task::Builder::spawn closure, as required.
                unsafe { TaskFuture::new_for_async_task(metadata, scheduler, fut) }
            },
            schedule,
        );
        runnable.schedule();
        task
    }
}

/// Trait for spawning a non-`Send` task on an executor.
pub trait SpawnLocal {
    /// Gets a scheduler for a new task.
    fn scheduler_local(&self, metadata: &TaskMetadata) -> Arc<dyn Schedule>;

    /// Spawns a task.
    #[track_caller]
    fn spawn_local<T: 'static>(
        &self,
        name: impl Into<Arc<str>>,
        fut: impl Future<Output = T> + 'static,
    ) -> Task<T>
    where
        Self: Sized,
    {
        let mut metadata = TaskMetadata::new(name.into());
        let scheduler = self.scheduler_local(&metadata);
        metadata.scheduler = Arc::downgrade(&scheduler);
        let (runnable, task) = async_task::Builder::new().metadata(metadata).spawn_local(
            |metadata| {
                // SAFETY: calling from the async_task::Builder::spawn closure, as required.
                unsafe { TaskFuture::new_for_async_task(metadata, scheduler, fut) }
            },
            schedule,
        );
        runnable.schedule();
        task
    }
}

thread_local! {
    static CURRENT_TASK: LoanCell<TaskMetadata> = const { LoanCell::new() };
}

/// Calls `f` with the current task metadata, if there is a current task.
pub fn with_current_task_metadata<F: FnOnce(Option<&TaskMetadata>) -> R, R>(f: F) -> R {
    CURRENT_TASK.with(|task| task.borrow(f))
}

impl<T: ?Sized + Spawn> Spawn for &'_ T {
    fn scheduler(&self, metadata: &TaskMetadata) -> Arc<dyn Schedule> {
        (*self).scheduler(metadata)
    }
}

impl<T: ?Sized + Spawn> Spawn for Box<T> {
    fn scheduler(&self, metadata: &TaskMetadata) -> Arc<dyn Schedule> {
        self.as_ref().scheduler(metadata)
    }
}

impl<T: ?Sized + Spawn> Spawn for Arc<T> {
    fn scheduler(&self, metadata: &TaskMetadata) -> Arc<dyn Schedule> {
        self.as_ref().scheduler(metadata)
    }
}

impl<T: ?Sized + SpawnLocal> SpawnLocal for &'_ T {
    fn scheduler_local(&self, metadata: &TaskMetadata) -> Arc<dyn Schedule> {
        (*self).scheduler_local(metadata)
    }
}

impl<T: ?Sized + SpawnLocal> SpawnLocal for Box<T> {
    fn scheduler_local(&self, metadata: &TaskMetadata) -> Arc<dyn Schedule> {
        self.as_ref().scheduler_local(metadata)
    }
}

impl<T: ?Sized + SpawnLocal> SpawnLocal for Arc<T> {
    fn scheduler_local(&self, metadata: &TaskMetadata) -> Arc<dyn Schedule> {
        self.as_ref().scheduler_local(metadata)
    }
}

const TASK_STATE_READY: u32 = 0;
const TASK_STATE_WAITING: u32 = 1;
const TASK_STATE_RUNNING: u32 = 2;
const TASK_STATE_DONE: u32 = 3;

/// The state of a task.
#[derive(Debug, Copy, Clone)]
#[repr(u64)]
pub enum TaskState {
    /// The task is ready to run.
    Ready,
    /// The task is waiting on some condition.
    Waiting,
    /// The task is running on an executor.
    Running,
    /// The task has completed.
    Complete,
    /// The task was cancelled before it completed.
    Cancelled,
}

impl Display for TaskState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            TaskState::Ready => "ready",
            TaskState::Waiting => "waiting",
            TaskState::Running => "running",
            TaskState::Complete => "complete",
            TaskState::Cancelled => "cancelled",
        };
        f.pad(s)
    }
}

/// A list of tasks.
pub struct TaskList {
    slab: Mutex<Slab<TaskMetadataPtr>>,
}

static TASK_LIST: TaskList = TaskList::new();

impl TaskList {
    const fn new() -> Self {
        Self {
            slab: Mutex::new(Slab::new()),
        }
    }

    /// Gets the global task list.
    pub fn global() -> &'static Self {
        &TASK_LIST
    }

    /// Gets a snapshot of the current tasks.
    pub fn tasks(&self) -> Vec<TaskData> {
        let tasks = self.slab.lock();
        tasks
            .iter()
            .map(|(id, task)| {
                // SAFETY: the pointer is guaranteed to be valid while the lock
                // is held, since it was published via
                // [`TaskMetadata::register`] and will be unpublished by
                // [`TaskMetadata::drop`].
                let task = unsafe { &*task.0 };
                let scheduler = task.scheduler.upgrade().map(|s| s.name());
                TaskData {
                    id,
                    name: task.name.clone(),
                    location: task.location,
                    state: task.state(),
                    executor: scheduler,
                }
            })
            .collect()
    }
}

/// Information about a task.
#[derive(Debug)]
pub struct TaskData {
    id: usize,
    name: Arc<str>,
    location: &'static Location<'static>,
    state: TaskState,
    executor: Option<Arc<str>>,
}

impl TaskData {
    /// The task's unique ID.
    ///
    /// This ID may be reused.
    pub fn id(&self) -> usize {
        self.id
    }

    /// The executor's name.
    pub fn executor(&self) -> Option<&str> {
        self.executor.as_deref()
    }

    /// The task's metadata.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The location where the task was spawned.
    pub fn location(&self) -> &'static Location<'static> {
        self.location
    }

    /// The state of the task.
    pub fn state(&self) -> TaskState {
        self.state
    }
}
