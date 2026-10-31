// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A simple asynchronous task model for execution that needs to be started,
//! stopped, mutated, and inspected.

#![forbid(unsafe_code)]

use inspect::Inspect;
use inspect::InspectMut;
use pal_async::task::Spawn;
use pal_async::task::Task;
use parking_lot::Mutex;
use std::future::Future;
use std::future::poll_fn;
use std::pin::Pin;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

/// A method implemented by a task that can be run and stopped, storing
/// transient state in `S`.
pub trait AsyncRun<S>: 'static + Send {
    /// Runs the task.
    ///
    /// The task should stop when `stop` becomes ready. This can be determined
    /// either by awaiting on `stop`, or by calling [`StopTask::until_stopped`]
    /// with a future to run.
    /// A future passed to `until_stopped` must return from each poll after
    /// bounded work. Otherwise, use [`StopTask::check`] at cancel-safe work
    /// boundaries within that future.
    ///
    /// The function should return `Ok(())` if the task is complete, in which
    /// case it will only run again after being removed and reinserted.
    ///
    /// If the function instead returns `Err(Cancelled)`, this indicates that
    /// the task's work is not complete, and it should be restarted after
    /// handling any incoming events. Implementations that choose to use
    /// [`StopTask::until_stopped`] must be cancel-tolerant: because
    /// [`StopTask::until_stopped`] drops the inner future when a stop
    /// is signaled, implementations must ensure no in-flight work is silently
    /// lost across a cancellation.
    fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        _: &mut S,
    ) -> impl Send + Future<Output = Result<(), Cancelled>>;
}

/// The return error from [`AsyncRun::run`] indicating the task has not yet
/// finished executing.
///
/// This is control flow, not an operational error. It deliberately does not
/// implement [`std::error::Error`], preventing implicit error boxing.
///
/// ```compile_fail
/// let error: Box<dyn std::error::Error> = Box::new(task_control::Cancelled);
/// ```
#[derive(Debug)]
pub struct Cancelled;

/// A future indicating that the task should return for event processing or
/// because the task was stopped.
pub struct StopTask<'a> {
    inner: &'a mut (dyn 'a + Send + Future<Output = ()> + Unpin),
    requested: &'a AtomicBool,
    outer_waker: Option<Waker>,
}

/// A control-request check that can be used while [`StopTask`] is borrowed by a wait.
///
/// The handle borrows the backing signal, not the `StopTask`, and must only be
/// used within the current worker activation.
#[derive(Clone, Copy)]
pub struct StopCheck<'a>(&'a AtomicBool);

impl StopCheck<'_> {
    /// Checks for a control request with only a relaxed atomic load.
    #[inline]
    pub fn check(&self) -> Result<(), Cancelled> {
        if self.0.load(Ordering::Relaxed) {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }
}

/// The inner polling implementation, which polls for an incoming request from
/// `TaskControl`.
///
/// This is separate from `StopTask` so that the types can be erased.
struct StopTaskInner<'a, T, S> {
    shared: &'a Shared<T, S>,
}

impl<T: AsyncRun<S>, S> Future for StopTaskInner<'_, T, S> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut shared = self.get_mut().shared.lock();
        if !shared.calls.is_empty() || shared.stop {
            return Poll::Ready(());
        }
        if shared
            .inner_waker
            .as_ref()
            .is_none_or(|waker| !cx.waker().will_wake(waker))
        {
            shared.inner_waker = Some(cx.waker().clone());
        }
        Poll::Pending
    }
}

impl<'a> StopTask<'a> {
    /// Runs `f` with a task that never receives control requests.
    ///
    /// Intended for test helpers that run a device without a `TaskControl`.
    pub async fn run_unstoppable<R>(f: impl AsyncFnOnce(&mut StopTask<'_>) -> R) -> R {
        let requested = AtomicBool::new(false);
        let mut pending = std::future::pending();
        let mut stop = StopTask {
            inner: &mut pending,
            requested: &requested,
            outer_waker: None,
        };
        f(&mut stop).await
    }

    /// Checks whether the task should return for stop or event processing.
    ///
    /// Call at cancel-safe boundaries in loops that can keep making progress
    /// without yielding. This does not yield or cancel an outstanding wait;
    /// use [`Self::until_stopped`] for waits that must be interruptible.
    ///
    /// This only performs a relaxed atomic load. Request producers publish
    /// the signal directly; checking does not register or poll a waker.
    #[inline]
    pub fn check(&self) -> Result<(), Cancelled> {
        StopCheck(self.requested).check()
    }

    /// Checks for a request and returns a handle for this worker activation.
    ///
    /// Use this inside a future passed to [`Self::until_stopped`], or when
    /// several processing loops need to check the same control request.
    pub fn check_handle(&self) -> Result<StopCheck<'a>, Cancelled> {
        self.check()?;
        Ok(StopCheck(self.requested))
    }

    /// Runs `fut` until the task is requested to stop.
    ///
    /// If `fut` completes, then `Ok(_)` is returned.
    ///
    /// If the task is requested to stop before `fut` completes, then `fut` is
    /// dropped and `Err(Cancelled)` is returned.
    /// Stop takes priority when both stop and work are ready.
    pub async fn until_stopped<F: Future>(&mut self, fut: F) -> Result<F::Output, Cancelled> {
        let mut fut = pin!(fut);

        // Since this is a common fast path, implement the select manually.
        poll_fn(|cx| {
            if let Err(cancelled) = self.check() {
                Poll::Ready(Err(cancelled))
            } else if let Poll::Ready(r) = fut.as_mut().poll(cx) {
                Poll::Ready(Ok(r))
            } else if Pin::new(&mut *self).poll(cx).is_ready() {
                Poll::Ready(Err(Cancelled))
            } else {
                Poll::Pending
            }
        })
        .await
    }
}

impl Future for StopTask<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.check().is_err() {
            return Poll::Ready(());
        }
        if self
            .outer_waker
            .as_ref()
            .is_none_or(|waker| !cx.waker().will_wake(waker))
        {
            if Pin::new(&mut self.inner).poll(cx).is_ready() {
                return Poll::Ready(());
            }
            self.outer_waker = Some(cx.waker().clone());
        }
        // Recheck after registering the waker so a concurrent request
        // cannot leave an interruptible wait asleep.
        if self.check().is_err() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

/// A task wrapper that runs a task asynchronously and provides access to its
/// state and control over its execution (start/stop).
///
/// Pairs a task implementation `T: AsyncRun<S>` with transient state `S`.
/// Execution is cancelled when [`stop`](Self::stop) is invoked.
///
/// Outside of an actual [`stop`](Self::stop), `TaskControl` also raises the
/// stop signal to fulfill [`update_with`](Self::update_with) and
/// [`Inspect`]/[`InspectMut`] calls, after which `run` is re-invoked with
/// the (possibly updated) state. See [`AsyncRun`] for the cancel-tolerance
/// requirements this places on implementations.
pub struct TaskControl<T, S> {
    inner: Inner<T, S>,
}

/// A trait for inspecting a task and its associated state.
pub trait InspectTask<S>: AsyncRun<S> {
    /// Inspects the task and its state.
    ///
    /// The state may be missing if it has not yet been inserted into the
    /// [`TaskControl`].
    fn inspect(&self, req: inspect::Request<'_>, state: Option<&S>);
}

impl<T: InspectTask<S>, S> Inspect for TaskAndState<T, S> {
    fn inspect(&self, req: inspect::Request<'_>) {
        self.task.inspect(req, self.state.as_ref());
    }
}

impl<T: InspectTask<S>, S> Inspect for TaskControl<T, S> {
    fn inspect(&self, req: inspect::Request<'_>) {
        match &self.inner {
            Inner::NoState(task_and_state) => task_and_state.inspect(req),
            Inner::WithState {
                activity, shared, ..
            } => match activity {
                Activity::Stopped(task_and_state) => task_and_state.inspect(req),
                Activity::Running => {
                    let deferred = req.defer();
                    Shared::push_call(
                        shared,
                        Box::new(|task_and_state| {
                            deferred.inspect(&task_and_state);
                        }),
                    )
                }
            },
            Inner::Invalid => unreachable!(),
        }
    }
}

/// A trait for mutably inspecting a task and its associated state.
pub trait InspectTaskMut<T>: AsyncRun<T> {
    /// Inspects the task and its state.
    ///
    /// The state may be missing if it has not yet been inserted into the
    /// [`TaskControl`].
    fn inspect_mut(&mut self, req: inspect::Request<'_>, state: Option<&mut T>);
}

impl<T: InspectTaskMut<S>, S> InspectMut for TaskAndState<T, S> {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        self.task.inspect_mut(req, self.state.as_mut());
    }
}

impl<T: InspectTaskMut<U>, U> InspectMut for TaskControl<T, U> {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        match &mut self.inner {
            Inner::NoState(task_and_state) => task_and_state.inspect_mut(req),
            Inner::WithState {
                activity, shared, ..
            } => match activity {
                Activity::Stopped(task_and_state) => task_and_state.inspect_mut(req),
                Activity::Running => {
                    let deferred = req.defer();
                    Shared::push_call(
                        shared,
                        Box::new(|task_and_state| {
                            deferred.inspect(task_and_state);
                        }),
                    );
                }
            },
            Inner::Invalid => unreachable!(),
        }
    }
}

type CallFn<T, S> = Box<dyn FnOnce(&mut TaskAndState<T, S>) + Send>;

enum Inner<T, S> {
    NoState(Box<TaskAndState<T, S>>),
    WithState {
        activity: Activity<T, S>,
        _backing_task: Task<()>,
        shared: Arc<Shared<T, S>>,
    },
    Invalid,
}

struct TaskAndState<T, S> {
    task: T,
    state: Option<S>,
    done: bool,
}

struct Shared<T, S> {
    // Mirrors stop or queued calls. Writers hold the state lock; readers use
    // this only to return control, not to acquire access to the protected state.
    requested: AtomicBool,
    inner: Mutex<SharedState<T, S>>,
}

struct SharedState<T, S> {
    task_and_state: Option<Box<TaskAndState<T, S>>>,
    calls: Vec<CallFn<T, S>>,
    stop: bool,
    outer_waker: Option<Waker>,
    inner_waker: Option<Waker>,
}

impl<T, S> Shared<T, S> {
    fn lock(&self) -> parking_lot::MutexGuard<'_, SharedState<T, S>> {
        self.inner.lock()
    }

    fn push_call(this: &Self, f: CallFn<T, S>) {
        let waker = {
            let mut shared = this.lock();
            shared.calls.push(f);
            this.requested.store(true, Ordering::Relaxed);
            shared.inner_waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

enum Activity<T, S> {
    Stopped(Box<TaskAndState<T, S>>),
    Running,
}

impl<T: AsyncRun<S>, S: 'static + Send> TaskControl<T, S> {
    /// Creates the task control, taking the state for the task but not yet
    /// creating or starting it.
    pub fn new(task: T) -> Self {
        Self {
            inner: Inner::NoState(Box::new(TaskAndState {
                task,
                state: None,
                done: false,
            })),
        }
    }

    /// Returns true if a task has been inserted.
    pub fn has_state(&self) -> bool {
        match &self.inner {
            Inner::NoState(_) => false,
            Inner::WithState { .. } => true,
            Inner::Invalid => unreachable!(),
        }
    }

    /// Returns true if a task is running.
    pub fn is_running(&self) -> bool {
        match &self.inner {
            Inner::NoState(_)
            | Inner::WithState {
                activity: Activity::Stopped { .. },
                ..
            } => false,
            Inner::WithState {
                activity: Activity::Running,
                ..
            } => true,
            Inner::Invalid => unreachable!(),
        }
    }

    /// Gets the task.
    ///
    /// Panics if the task is running.
    #[track_caller]
    pub fn task(&self) -> &T {
        self.get().0
    }

    /// Gets the task.
    ///
    /// Panics if the task is running.
    #[track_caller]
    pub fn task_mut(&mut self) -> &mut T {
        self.get_mut().0
    }

    /// Gets the transient task state.
    ///
    /// Panics if the task is running.
    #[track_caller]
    pub fn state(&self) -> Option<&S> {
        self.get().1
    }

    /// Gets the transient task state.
    ///
    /// Panics if the task is running.
    #[track_caller]
    pub fn state_mut(&mut self) -> Option<&mut S> {
        self.get_mut().1
    }

    /// Gets the task and its state.
    ///
    /// Panics if the task is running.
    #[track_caller]
    pub fn get(&self) -> (&T, Option<&S>) {
        let task_and_state = match &self.inner {
            Inner::NoState(task_and_state) => task_and_state,
            Inner::WithState {
                activity: Activity::Stopped(task_and_state),
                ..
            } => task_and_state,
            Inner::WithState {
                activity: Activity::Running,
                ..
            } => panic!("attempt to access running task"),
            Inner::Invalid => unreachable!(),
        };
        (&task_and_state.task, task_and_state.state.as_ref())
    }

    /// Gets the state and the task.
    ///
    /// Panics if the task is running.
    #[track_caller]
    pub fn get_mut(&mut self) -> (&mut T, Option<&mut S>) {
        let task_and_state = match &mut self.inner {
            Inner::NoState(task_and_state) => task_and_state,
            Inner::WithState {
                activity: Activity::Stopped(task_and_state),
                ..
            } => task_and_state,
            Inner::WithState {
                activity: Activity::Running,
                ..
            } => panic!("attempt to access running task"),
            Inner::Invalid => unreachable!(),
        };
        (&mut task_and_state.task, task_and_state.state.as_mut())
    }

    /// Retrieves the task and its state.
    ///
    /// Panics if the task is running.
    #[track_caller]
    pub fn into_inner(self) -> (T, Option<S>) {
        let task_and_state = match self.inner {
            Inner::NoState(task_and_state) => task_and_state,
            Inner::WithState {
                activity: Activity::Stopped(task_and_state),
                ..
            } => task_and_state,
            Inner::WithState {
                activity: Activity::Running,
                ..
            } => panic!("attempt to extract running task"),
            Inner::Invalid => unreachable!(),
        };
        (task_and_state.task, task_and_state.state)
    }

    /// Calls `f` against the task and its state.
    ///
    /// If the task is running, then `f` will run remotely and will not
    /// necessarily finish before this routine returns.
    pub fn update_with(&mut self, f: impl 'static + Send + FnOnce(&mut T, Option<&mut S>)) {
        let f = |task_and_state: &mut TaskAndState<T, S>| {
            f(&mut task_and_state.task, task_and_state.state.as_mut())
        };
        match &mut self.inner {
            Inner::NoState(task_and_state) => f(task_and_state),
            Inner::WithState {
                activity, shared, ..
            } => match activity {
                Activity::Stopped(task_and_state) => f(task_and_state),
                Activity::Running => Shared::push_call(shared, Box::new(f)),
            },
            Inner::Invalid => unreachable!(),
        }
    }

    /// Inserts the state the task object will use to run and starts the backing
    /// task, but does not start running it.
    #[track_caller]
    pub fn insert(&mut self, spawn: impl Spawn, name: impl Into<Arc<str>>, state: S) -> &mut S {
        self.inner = match std::mem::replace(&mut self.inner, Inner::Invalid) {
            Inner::NoState(mut task_and_state) => {
                task_and_state.state = Some(state);
                task_and_state.done = false;
                let shared = Arc::new(Shared {
                    requested: AtomicBool::new(true),
                    inner: Mutex::new(SharedState {
                        task_and_state: None,
                        calls: Vec::new(),
                        stop: true,
                        outer_waker: None,
                        inner_waker: None,
                    }),
                });
                let backing_task = spawn.spawn(name, Self::run(shared.clone()));
                Inner::WithState {
                    activity: Activity::Stopped(task_and_state),
                    _backing_task: backing_task,
                    shared,
                }
            }
            Inner::WithState { .. } => panic!("attempt to insert already-present state"),
            Inner::Invalid => unreachable!(),
        };
        self.state_mut().unwrap()
    }

    /// Starts the task if it is not already running.
    ///
    /// Returns true if the task is now running (even if it was previously
    /// running). Returns false if the task is not running (either because its
    /// state has not been inserted, or because it has already completed).
    pub fn start(&mut self) -> bool {
        match &mut self.inner {
            Inner::WithState {
                activity, shared, ..
            } => match std::mem::replace(activity, Activity::Running) {
                Activity::Stopped(task_and_state) => {
                    if task_and_state.done {
                        *activity = Activity::Stopped(task_and_state);
                        return false;
                    }
                    let waker = {
                        let mut state = shared.lock();
                        state.task_and_state = Some(task_and_state);
                        state.stop = false;
                        shared
                            .requested
                            .store(!state.calls.is_empty(), Ordering::Relaxed);
                        state.inner_waker.take()
                    };
                    if let Some(waker) = waker {
                        waker.wake();
                    }
                    true
                }
                Activity::Running => true,
            },
            Inner::NoState(_) => false,
            Inner::Invalid => {
                unreachable!()
            }
        }
    }

    async fn run(shared: Arc<Shared<T, S>>) {
        let mut stop_source = StopTaskInner { shared: &shared };
        let mut stop_task = StopTask {
            inner: &mut stop_source,
            requested: &shared.requested,
            outer_waker: None,
        };
        let mut calls = Vec::new();
        loop {
            let (mut task_and_state, stop) = poll_fn(|cx| {
                let mut state = shared.lock();
                let has_work = state
                    .task_and_state
                    .as_ref()
                    .is_some_and(|ts| !state.calls.is_empty() || (!state.stop && !ts.done));
                if !has_work {
                    state.inner_waker = Some(cx.waker().clone());
                    return Poll::Pending;
                }
                calls.append(&mut state.calls);
                // Drain and reset under the producer lock. Requests arriving
                // while these callbacks run must remain visible to the worker.
                shared.requested.store(state.stop, Ordering::Relaxed);
                Poll::Ready((state.task_and_state.take().unwrap(), state.stop))
            })
            .await;

            for call in calls.drain(..) {
                call(&mut task_and_state);
            }

            if !stop && !task_and_state.done {
                // The driver may have consumed or replaced the registered
                // worker waker since the preceding activation.
                stop_task.outer_waker = None;
                if stop_task.check().is_ok() {
                    task_and_state.done = task_and_state
                        .task
                        .run(&mut stop_task, task_and_state.state.as_mut().unwrap())
                        .await
                        .is_ok();
                }
            }

            let waker = {
                let mut shared = shared.lock();
                shared.task_and_state = Some(task_and_state);
                shared.outer_waker.take()
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }

    /// Poll variant of [`stop`](Self::stop). Signals the task to stop and polls
    /// for completion.
    ///
    /// Returns `Poll::Ready(true)` if the task was previously running and has
    /// now stopped. Returns `Poll::Ready(false)` if the task was not running,
    /// not inserted, or had already completed. Returns `Poll::Pending` if the
    /// task has not yet stopped.
    pub fn poll_stop(&mut self, cx: &mut Context<'_>) -> Poll<bool> {
        match &mut self.inner {
            Inner::WithState {
                activity, shared, ..
            } => match activity {
                Activity::Running => {
                    let mut state = shared.lock();
                    state.stop = true;
                    shared.requested.store(true, Ordering::Relaxed);
                    if state.task_and_state.is_none() || !state.calls.is_empty() {
                        state.outer_waker = Some(cx.waker().clone());
                        let waker = state.inner_waker.take();
                        drop(state);
                        if let Some(waker) = waker {
                            waker.wake();
                        }
                        return Poll::Pending;
                    }
                    let task_and_state = state.task_and_state.take().unwrap();
                    drop(state);
                    let done = task_and_state.done;
                    *activity = Activity::Stopped(task_and_state);
                    Poll::Ready(!done)
                }
                _ => Poll::Ready(false),
            },
            Inner::NoState(_) => Poll::Ready(false),
            Inner::Invalid => unreachable!(),
        }
    }

    /// Stops the task, waiting for it to be cancelled.
    ///
    /// Returns true if the task was previously running. Returns false if the
    /// task was not running, not inserted, or had already completed.
    pub async fn stop(&mut self) -> bool {
        poll_fn(|cx| self.poll_stop(cx)).await
    }

    /// Removes the task state.
    ///
    /// Panics if the task is not stopped.
    #[track_caller]
    pub fn remove(&mut self) -> S {
        match std::mem::replace(&mut self.inner, Inner::Invalid) {
            Inner::WithState {
                activity: Activity::Stopped(mut task_and_state),
                ..
            } => {
                let state = task_and_state.state.take().unwrap();
                self.inner = Inner::NoState(task_and_state);
                state
            }
            Inner::NoState(_) => panic!("attempt to remove missing state"),
            Inner::WithState { .. } => panic!("attempt to remove state from running task"),
            Inner::Invalid => {
                unreachable!()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AsyncRun;
    use crate::Cancelled;
    use crate::StopTask;
    use crate::TaskControl;
    use futures::FutureExt;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use parking_lot::Mutex;
    use std::future::Future;
    use std::pin::Pin;
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Wake;
    use std::task::Waker;

    #[derive(Default)]
    struct CheckSignal {
        requested: AtomicBool,
        polls: AtomicUsize,
        waker: Mutex<Option<Waker>>,
        request_during_registration: AtomicBool,
    }

    impl CheckSignal {
        fn notify(&self) {
            let waker = self.waker.lock().clone();
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }

    struct CheckStop<'a>(&'a CheckSignal);

    impl Future for CheckStop<'_> {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            self.0.polls.fetch_add(1, Ordering::Relaxed);
            let old = self.0.waker.lock().replace(cx.waker().clone());
            drop(old);
            let requested = self.0.requested.load(Ordering::Relaxed);
            if self
                .0
                .request_during_registration
                .swap(false, Ordering::Relaxed)
            {
                self.0.requested.store(true, Ordering::Relaxed);
            }
            if requested {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }

    async fn run_with_signal<R>(
        signal: &CheckSignal,
        f: impl AsyncFnOnce(&mut StopTask<'_>) -> R,
    ) -> R {
        let mut source = CheckStop(signal);
        let mut stop = StopTask {
            inner: &mut source,
            requested: &signal.requested,
            outer_waker: None,
        };
        f(&mut stop).await
    }

    #[test]
    fn checks_observe_requests_without_polling_or_waker_registration() {
        let signal = Arc::new(CheckSignal::default());
        let result = run_with_signal(&signal, async |stop| {
            let check = stop.check_handle().unwrap();
            for _ in 0..1000 {
                stop.check().unwrap();
                check.check().unwrap();
                stop.until_stopped(std::future::ready(())).await.unwrap();
            }
            assert_eq!(signal.polls.load(Ordering::Relaxed), 0);
            assert!(signal.waker.lock().is_none());
            assert!(stop.outer_waker.is_none());

            signal.notify();
            check.check().unwrap();
            stop.check().unwrap();

            signal.requested.store(true, Ordering::Relaxed);
            assert!(check.check().is_err());
            assert!(stop.check().is_err());
            assert!(stop.check().is_err());
            assert_eq!(signal.polls.load(Ordering::Relaxed), 0);
            signal.requested.store(false, Ordering::Relaxed);
            stop.check().unwrap();
            signal.requested.store(true, Ordering::Relaxed);
            assert!(stop.check().is_err());
        })
        .now_or_never();
        assert!(result.is_some());
    }

    #[test]
    fn interruptible_wait_prioritizes_stop_and_handles_noop_wakers() {
        let signal = Arc::new(CheckSignal::default());
        let result = run_with_signal(&signal, async |stop| {
            {
                let mut wait = pin!(stop.until_stopped(std::future::pending::<()>()));
                assert!(
                    wait.as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
                signal.requested.store(true, Ordering::Relaxed);
                signal.notify();
                assert!(matches!(
                    wait.as_mut().poll(&mut Context::from_waker(Waker::noop())),
                    Poll::Ready(Err(_))
                ));
            }
            let work = std::future::poll_fn(|_| -> Poll<u32> {
                panic!("ready stop must prevent polling work");
            });
            assert!(stop.until_stopped(work).await.is_err());
            assert!(stop.check().is_err());
        })
        .now_or_never();
        assert!(result.is_some());
    }

    #[test]
    fn wait_caches_registration_and_tracks_executor_waker_changes() {
        #[derive(Default)]
        struct CountWake(AtomicUsize);
        impl Wake for CountWake {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let signal = Arc::new(CheckSignal::default());
        let count = Arc::new(CountWake::default());
        let waker = Waker::from(count.clone());
        let result = run_with_signal(&signal, async |stop| {
            let mut cx = Context::from_waker(&waker);
            assert!(Pin::new(&mut *stop).poll(&mut cx).is_pending());
            signal.notify();
            stop.check().unwrap();
            assert!(Pin::new(&mut *stop).poll(&mut cx).is_pending());
            assert_eq!(signal.polls.load(Ordering::Relaxed), 1);
            let new_count = Arc::new(CountWake::default());
            let new_waker = Waker::from(new_count.clone());
            let mut cx = Context::from_waker(&new_waker);
            assert!(Pin::new(&mut *stop).poll(&mut cx).is_pending());
            assert_eq!(signal.polls.load(Ordering::Relaxed), 2);
            signal.requested.store(true, Ordering::Relaxed);
            signal.notify();
            assert_eq!(count.0.load(Ordering::Relaxed), 1);
            assert_eq!(new_count.0.load(Ordering::Relaxed), 1);
            assert!(Pin::new(&mut *stop).poll(&mut cx).is_ready());
            signal.requested.store(false, Ordering::Relaxed);
            signal.waker.lock().take();
            stop.outer_waker = None;
            assert!(Pin::new(&mut *stop).poll(&mut cx).is_pending());
            assert_eq!(signal.polls.load(Ordering::Relaxed), 3);
        })
        .now_or_never();
        assert!(result.is_some());
    }

    #[test]
    fn wait_observes_request_arriving_during_registration() {
        let signal = CheckSignal {
            request_during_registration: AtomicBool::new(true),
            ..Default::default()
        };
        let result = run_with_signal(&signal, async |stop| {
            let mut wait = pin!(stop.until_stopped(std::future::pending::<()>()));
            assert!(matches!(
                wait.as_mut().poll(&mut Context::from_waker(Waker::noop())),
                Poll::Ready(Err(_))
            ));
        })
        .now_or_never();
        assert!(result.is_some());
    }

    #[async_test]
    async fn checks_interrupt_busy_workers_for_update_stop_and_restart(driver: DefaultDriver) {
        struct Busy {
            started: std::sync::mpsc::Sender<usize>,
            updates: usize,
        }
        impl AsyncRun<()> for Busy {
            async fn run(&mut self, stop: &mut StopTask<'_>, _: &mut ()) -> Result<(), Cancelled> {
                self.started.send(self.updates).unwrap();
                loop {
                    stop.check()?;
                    std::future::ready(()).await;
                }
            }
        }
        async fn stop_with_timeout(task: &mut TaskControl<Busy, ()>, driver: &DefaultDriver) {
            let mut timer = pal_async::timer::PolledTimer::new(driver);
            match futures::future::select(
                pin!(task.stop()),
                pin!(timer.sleep(std::time::Duration::from_secs(5))),
            )
            .await
            {
                futures::future::Either::Left((stopped, _)) => assert!(stopped),
                futures::future::Either::Right(_) => panic!("busy worker did not stop"),
            }
        }

        let (_thread, worker_driver) = pal_async::DefaultPool::spawn_on_thread("busy-check-test");
        let (started, recv_started) = std::sync::mpsc::channel();
        let mut task = TaskControl::new(Busy {
            started,
            updates: 0,
        });
        task.insert(&worker_driver, "busy", ());
        assert!(task.start());
        recv_started
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let (entered, recv_entered) = std::sync::mpsc::channel();
        let (release, recv_release) = std::sync::mpsc::channel();
        task.update_with(move |worker, _| {
            worker.updates += 1;
            entered.send(()).unwrap();
            recv_release
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
        });
        recv_entered
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        task.update_with(|worker, _| worker.updates += 1);
        release.send(()).unwrap();
        let updates_at_restart = recv_started
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        stop_with_timeout(&mut task, &driver).await;
        assert_eq!(updates_at_restart, 2);
        assert_eq!(task.task().updates, 2);
        assert!(task.start());
        recv_started
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        stop_with_timeout(&mut task, &driver).await;
        assert_eq!(task.task().updates, 2);
    }

    struct Foo(u32);

    impl AsyncRun<bool> for Foo {
        async fn run(
            &mut self,
            stop: &mut StopTask<'_>,
            state: &mut bool,
        ) -> Result<(), Cancelled> {
            stop.until_stopped(async {
                self.0 += 1;
                if !*state {
                    std::future::pending::<()>().await;
                }
            })
            .await
        }
    }

    async fn yield_once() {
        let mut yielded = false;
        std::future::poll_fn(|cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await
    }

    #[async_test]
    async fn test(driver: DefaultDriver) {
        let mut t = TaskControl::new(Foo(5));
        t.insert(&driver, "test", false);
        t.remove();
        t.insert(&driver, "test", false);
        assert_eq!(t.task().0, 5);
        assert!(t.start());
        yield_once().await;
        assert!(t.stop().await);
        assert_eq!(t.task().0, 6);
        *t.state_mut().unwrap() = true;
        assert!(t.start());
        yield_once().await;
        assert!(!t.stop().await);
        assert_eq!(t.task().0, 7);
        // The task has completed, so starting it again will not increment the counter.
        assert!(!t.start());
        yield_once().await;
        assert!(!t.stop().await);
        assert_eq!(t.task().0, 7);
    }

    #[async_test]
    async fn test_cancelled_stop(driver: DefaultDriver) {
        let mut t = TaskControl::new(Foo(5));
        t.insert(&driver, "test", false);
        assert!(t.start());
        yield_once().await;
        t.update_with(|t, _| t.0 += 1);
        assert!(t.stop().now_or_never().is_none());
        t.update_with(|t, _| t.0 += 1);
        assert!(t.stop().await);
        assert_eq!(t.task_mut().0, 8);
    }

    #[async_test]
    async fn test_poll_stop(driver: DefaultDriver) {
        let mut t = TaskControl::new(Foo(5));

        // poll_stop on a task without state returns Ready(false).
        assert_eq!(
            std::future::poll_fn(|cx| Poll::Ready(t.poll_stop(cx))).await,
            Poll::Ready(false)
        );

        t.insert(&driver, "test", false);

        // poll_stop on a stopped (not started) task returns Ready(false).
        assert_eq!(
            std::future::poll_fn(|cx| Poll::Ready(t.poll_stop(cx))).await,
            Poll::Ready(false)
        );

        assert!(t.start());
        yield_once().await;

        // poll_stop drives the task to stop, equivalent to stop().await.
        let result = std::future::poll_fn(|cx| t.poll_stop(cx)).await;
        assert!(result); // was running
        assert_eq!(t.task().0, 6);

        // poll_stop after already stopped returns Ready(false).
        assert_eq!(
            std::future::poll_fn(|cx| Poll::Ready(t.poll_stop(cx))).await,
            Poll::Ready(false)
        );
    }
}
