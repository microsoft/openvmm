// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A local executor, for running a single task with IO on the current thread.

use self::timer::Timer;
use crate::sys::local as sys;
use crate::timer::Instant;
use crate::timer::PollTimer;
use crate::timer::TimerDriver;
use crate::timer::TimerQueue;
use crate::timer::TimerResult;
use crate::waker::WakerList;
use futures::task::ArcWake;
use futures::task::waker_ref;
use parking_lot::Condvar;
use parking_lot::MappedMutexGuard;
use parking_lot::Mutex;
use parking_lot::MutexGuard;
use std::future::Future;
use std::pin::Pin;
use std::pin::pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

/// Blocks the current thread until the given future completes.
pub fn block_on<Fut>(fut: Fut) -> Fut::Output
where
    Fut: Future,
{
    block_with_io(|_| fut)
}

/// Polls a future that needs to issue IO until it completes.
pub fn block_with_io<F, R>(f: F) -> R
where
    F: AsyncFnOnce(LocalDriver) -> R,
{
    let mut executor = LocalExecutor::new();
    let fut = f(executor.driver());
    executor.run_until(pin!(fut))
}

/// An executor that runs on a single thread and runs only one future.
struct LocalExecutor {
    inner: Arc<LocalInner>,
}

impl LocalExecutor {
    fn new() -> Self {
        Self {
            inner: Arc::new(LocalInner::default()),
        }
    }

    fn driver(&self) -> LocalDriver {
        LocalDriver {
            inner: self.inner.clone(),
        }
    }

    fn run_until<F: Future>(&mut self, mut fut: Pin<&mut F>) -> F::Output {
        let waker = waker_ref(&self.inner);
        let mut cx = Context::from_waker(&waker);
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(r) => break r,
                Poll::Pending => self.inner.wait(),
            }
        }
    }
}

/// An IO driver for single-task use on a single thread.
#[derive(Debug, Clone)]
pub struct LocalDriver {
    pub(crate) inner: Arc<LocalInner>,
}

#[derive(Default, Debug)]
pub(crate) struct LocalInner {
    state: Mutex<LocalState>,
    wait_state: Mutex<sys::WaitState>,
    condvar: Condvar,
    wait_cancel: sys::WaitCancel,
}

#[derive(Debug, PartialEq, Eq, Default)]
enum OpState {
    // The executor is running.
    #[default]
    Running,
    // The executor should poll its task again without waiting.
    RunAgain,
    // The executor is waiting on IO.
    Waiting,
    // The executor wait has been cancelled.
    Woken,
}

#[derive(Debug, Default)]
struct LocalState {
    op_state: OpState,
    state_waiters: usize,
    sys: sys::State,
    timers: TimerQueue,
}

impl LocalInner {
    pub fn lock_sys_state(&self) -> MappedMutexGuard<'_, sys::State> {
        MutexGuard::map(self.lock_state(), |x| &mut x.sys)
    }

    // Locks the state for mutation.
    //
    // If the executor is currently waiting, then wakes up the executor first to
    // ensure that the executor never sees state changes between pre_wait and
    // post_wait.
    fn lock_state(&self) -> MutexGuard<'_, LocalState> {
        let mut guard = self.state.lock();

        match guard.op_state {
            OpState::Running | OpState::RunAgain => return guard,
            OpState::Waiting => {
                guard.state_waiters += 1;
                guard.op_state = OpState::Woken;
                drop(guard);
                self.wait_cancel.cancel_wait();
                guard = self.state.lock();
            }
            OpState::Woken => {
                guard.state_waiters += 1;
            }
        };
        self.condvar
            .wait_while(&mut guard, |state| state.op_state == OpState::Woken);
        assert_ne!(guard.op_state, OpState::Waiting);
        guard.state_waiters -= 1;
        if guard.state_waiters == 0 {
            // Notify the executor that it can proceed with its wait after the
            // mutex guard is dropped.
            self.condvar.notify_all();
        }
        guard
    }

    fn wait(&self) {
        let mut state = self.state.lock();
        // Wait until any threads that want to manipulate the state have
        // done so.
        self.condvar.wait_while(&mut state, |state| {
            state.op_state == OpState::Running && state.state_waiters > 0
        });
        if state.op_state != OpState::Running {
            assert_eq!(state.op_state, OpState::RunAgain);
            state.op_state = OpState::Running;
            return;
        }

        let mut wait_state = self
            .wait_state
            .try_lock()
            .expect("wait should not be called concurrently");

        state.sys.pre_wait(&mut wait_state, &self.wait_cancel);

        let timeout = state.timers.next_deadline().map(|deadline| {
            let now = Instant::now();
            deadline.max(now) - now
        });

        {
            state.op_state = OpState::Waiting;
            drop(state);
            wait_state.wait(&self.wait_cancel, timeout);
            state = self.state.lock();
            state.op_state = OpState::Running;
        }

        let mut wakers = WakerList::default();
        state.sys.post_wait(&mut wait_state, &mut wakers);
        state.timers.wake_expired(&mut wakers);
        drop(state);
        wakers.wake();
        // Notify mutators that the wait has finished.
        self.condvar.notify_all();
    }
}

impl ArcWake for LocalInner {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        let mut state = arc_self.state.lock();
        match state.op_state {
            OpState::Running => state.op_state = OpState::RunAgain,
            OpState::RunAgain => {}
            OpState::Waiting => {
                state.op_state = OpState::Woken;
                drop(state);
                arc_self.wait_cancel.cancel_wait();
            }
            OpState::Woken => {}
        }
    }
}

// Use a separate module so that `Timer` is not visible.
mod timer {
    use super::LocalInner;
    use crate::timer::TimerQueueId;
    use std::sync::Arc;

    #[derive(Debug)]
    pub struct Timer {
        pub(super) inner: Arc<LocalInner>,
        pub(super) id: TimerQueueId,
    }
}

impl TimerDriver for LocalDriver {
    type Timer = Timer;

    fn new_timer(&self) -> Self::Timer {
        let id = self.inner.lock_state().timers.add();
        Timer {
            inner: self.inner.clone(),
            id,
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        let _waker = self.inner.lock_state().timers.remove(self.id);
    }
}

impl PollTimer for Timer {
    fn poll_timer(&mut self, cx: &mut Context<'_>, deadline: Option<Instant>) -> Poll<Instant> {
        let mut state = self.inner.lock_state();
        if let Some(deadline) = deadline {
            state.timers.set_deadline(self.id, deadline);
        }
        match state.timers.poll_deadline(cx, self.id) {
            TimerResult::TimedOut(now) => Poll::Ready(now),
            TimerResult::Pending(_) => Poll::Pending,
        }
    }

    fn set_deadline(&mut self, deadline: Instant) {
        self.inner
            .lock_state()
            .timers
            .set_deadline(self.id, deadline);
    }
}

#[cfg(test)]
mod tests {
    use super::block_with_io;
    use crate::executor_tests;

    #[test]
    fn waker_works() {
        block_with_io(|_| executor_tests::waker_tests())
    }

    #[test]
    fn sleep_works() {
        block_with_io(executor_tests::sleep_tests)
    }

    #[test]
    fn wait_works() {
        block_with_io(executor_tests::wait_tests)
    }

    #[test]
    fn socket_works() {
        block_with_io(executor_tests::socket_tests)
    }

    #[cfg(windows)]
    #[test]
    fn overlapped_file_works() {
        block_with_io(executor_tests::windows::overlapped_file_tests)
    }
}
