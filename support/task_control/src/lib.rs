// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A simple asynchronous task model for execution that needs to be started,
//! stopped, mutated, and inspected.

#![forbid(unsafe_code)]

use fast_select::FastSelect;
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
    ///
    /// The function should return `Ok(())` if the task is complete, in which
    /// case it will only run again after being removed and reinserted.
    ///
    /// If the function instead returns `Err(Cancelled)`, this indicates that
    /// the task's work is not complete, and it should be restarted after
    /// handling any incoming events.
    fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        _: &mut S,
    ) -> impl Send + Future<Output = Result<(), Cancelled>>;
}

/// The return error from [`AsyncRun::run`] indicating the task has not yet
/// finished executing.
#[derive(Debug)]
pub struct Cancelled;

/// A future indicating that the task should return for event processing or
/// because the task was stopped.
pub struct StopTask<'a> {
    inner: &'a mut (dyn 'a + Send + Future<Output = ()> + Unpin),
    fast_select: &'a mut FastSelect,
}

/// The inner polling implementation, which polls for an incoming request from
/// `TaskControl`.
///
/// This is separate from `StopTask` so that the types can be erased.
struct StopTaskInner<'a, T, S> {
    shared: &'a Mutex<Shared<T, S>>,
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

impl StopTask<'_> {
    /// Runs `f`, providing access to `stop` via a `StopTask` passed to `f`.
    pub async fn run_with<R>(
        mut stop: impl Send + Future<Output = ()> + Unpin,
        f: impl AsyncFnOnce(&mut StopTask<'_>) -> R,
    ) -> R {
        let mut fast_select: FastSelect = FastSelect::new();
        let mut stop = StopTask {
            inner: &mut stop,
            fast_select: &mut fast_select,
        };
        f(&mut stop).await
    }

    /// Runs `fut` until the task is requested to stop.
    ///
    /// If `fut` completes, then `Ok(_)` is returned.
    ///
    /// If the task is requested to stop before `fut` completes, then `fut` is
    /// dropped and `Err(Cancelled)` is returned.
    pub async fn until_stopped<F: Future>(&mut self, fut: F) -> Result<F::Output, Cancelled> {
        // Wrap the cancel task in a FastSelect to avoid taking the channel lock
        // at each wakeup.
        let mut cancel = pin!(
            self.fast_select
                .select((poll_fn(|cx| Pin::new(&mut self.inner).poll(cx)),))
        );

        let mut fut = pin!(fut);

        // Since this is a common fast path, implement the select manually.
        poll_fn(|cx| {
            if let Poll::Ready(r) = fut.as_mut().poll(cx) {
                Poll::Ready(Ok(r))
            } else if cancel.as_mut().poll(cx).is_ready() {
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
        Pin::new(&mut self.inner).poll(cx)
    }
}

/// A task wrapper that runs the task asynchronously and provides access to its
/// state.
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
        shared: Arc<Mutex<Shared<T, S>>>,
    },
    Invalid,
}

struct TaskAndState<T, S> {
    task: T,
    state: Option<S>,
    done: bool,
}

struct Shared<T, S> {
    task_and_state: Option<Box<TaskAndState<T, S>>>,
    calls: Vec<CallFn<T, S>>,
    stop: bool,
    outer_waker: Option<Waker>,
    inner_waker: Option<Waker>,
}

impl<T, S> Shared<T, S> {
    fn push_call(this: &Mutex<Self>, f: CallFn<T, S>) {
        let waker = {
            let mut this = this.lock();
            this.calls.push(f);
            this.inner_waker.take()
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
                let shared = Arc::new(Mutex::new(Shared {
                    task_and_state: None,
                    calls: Vec::new(),
                    stop: true,
                    outer_waker: None,
                    inner_waker: None,
                }));
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
                        let mut shared = shared.lock();
                        shared.task_and_state = Some(task_and_state);
                        shared.stop = false;
                        shared.inner_waker.take()
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

    async fn run(shared: Arc<Mutex<Shared<T, S>>>) {
        StopTask::run_with(StopTaskInner { shared: &shared }, async |stop_task| {
            let mut calls = Vec::new();
            loop {
                let (mut task_and_state, stop) = poll_fn(|cx| {
                    let mut shared = shared.lock();
                    let has_work = shared
                        .task_and_state
                        .as_ref()
                        .is_some_and(|ts| !shared.calls.is_empty() || (!shared.stop && !ts.done));
                    if !has_work {
                        shared.inner_waker = Some(cx.waker().clone());
                        return Poll::Pending;
                    }
                    calls.append(&mut shared.calls);
                    Poll::Ready((shared.task_and_state.take().unwrap(), shared.stop))
                })
                .await;

                for call in calls.drain(..) {
                    call(&mut task_and_state);
                }

                if !stop && !task_and_state.done {
                    task_and_state.done = task_and_state
                        .task
                        .run(&mut *stop_task, task_and_state.state.as_mut().unwrap())
                        .await
                        .is_ok();
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
        })
        .await
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
                    let mut shared = shared.lock();
                    shared.stop = true;
                    if shared.task_and_state.is_none() || !shared.calls.is_empty() {
                        shared.outer_waker = Some(cx.waker().clone());
                        let waker = shared.inner_waker.take();
                        drop(shared);
                        if let Some(waker) = waker {
                            waker.wake();
                        }
                        return Poll::Pending;
                    }
                    let task_and_state = shared.task_and_state.take().unwrap();
                    drop(shared);
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
    use std::task::Poll;

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
