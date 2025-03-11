// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! An executor based on epoll.

use super::wait::FdWait;
use crate::fd::FdReadyDriver;
use crate::fd::PollFdReady;
use crate::interest::InterestSlot;
use crate::interest::PollEvents;
use crate::interest::PollInterestSet;
use crate::io_pool::IoBackend;
use crate::io_pool::IoDriver;
use crate::io_pool::IoPool;
use crate::timer::Instant;
use crate::timer::PollTimer;
use crate::timer::TimerDriver;
use crate::timer::TimerQueue;
use crate::timer::TimerQueueId;
use crate::timer::TimerResult;
use crate::wait::WaitDriver;
use crate::waker::WakerList;
use futures::FutureExt;
use futures::task::ArcWake;
use futures::task::waker_ref;
use pal::unix::Errno;
use pal::unix::SyscallResult;
use pal::unix::while_eintr;
use pal_event::Event;
use parking_lot::Mutex;
use std::fs::File;
use std::io;
use std::os::unix::prelude::*;
use std::pin::pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

/// An single-threaded task pool backed by epoll.
pub type EpollPool = IoPool<EpollBackend>;

/// A driver to spawn tasks and IO objects on [`EpollPool`].
pub type EpollDriver = IoDriver<EpollBackend>;

#[derive(Debug)]
pub struct EpollBackend {
    epfd: EpollFd,
    wake_event: Event,
    state: Mutex<EpollState>,
}

impl Default for EpollBackend {
    fn default() -> Self {
        let epfd = EpollFd::new().expect("epoll not functional");
        let wake_event = Event::new();
        // Register for notifications when the wake event is signaled. Use
        // edge-triggered mode because we can (we immediately consume the event
        // when it is signaled) and because miri requires it.
        epfd.add(
            wake_event.as_fd().as_raw_fd(),
            libc::EPOLLIN | libc::EPOLLET,
            0,
        )
        .expect("could not add wake event");
        Self {
            epfd,
            wake_event,
            state: Mutex::new(EpollState {
                state: PoolState::Running,
                timers: TimerQueue::default(),
                fd_ready_to_delete: Vec::new(),
            }),
        }
    }
}

#[derive(Debug)]
struct EpollState {
    state: PoolState,
    timers: TimerQueue,
    fd_ready_to_delete: Vec<Arc<FdReadyOp>>,
}

#[derive(Debug)]
enum PoolState {
    Running,
    RunAgain,
    Sleeping(Option<Instant>),
    Waking,
}

impl PoolState {
    fn reset(&mut self) {
        match self {
            PoolState::Running => {}
            PoolState::RunAgain => {}
            PoolState::Sleeping(_) => unreachable!(),
            PoolState::Waking => {}
        }
        *self = PoolState::Running;
    }

    /// Returns true if the wake event must be signaled.
    #[must_use]
    fn wake(&mut self) -> bool {
        match self {
            PoolState::Running => {
                *self = PoolState::RunAgain;
                false
            }
            PoolState::RunAgain => false,
            PoolState::Sleeping(_) => {
                *self = PoolState::Waking;
                true
            }
            PoolState::Waking => false,
        }
    }

    fn can_sleep(&self) -> bool {
        match self {
            PoolState::Running => true,
            PoolState::RunAgain => false,
            PoolState::Sleeping(_) => unreachable!(),
            PoolState::Waking => unreachable!(),
        }
    }

    fn sleep(&mut self, deadline: Option<Instant>) {
        match self {
            PoolState::Running => {}
            PoolState::RunAgain => unreachable!(),
            PoolState::Sleeping(_) => unreachable!(),
            PoolState::Waking => unreachable!(),
        }
        *self = PoolState::Sleeping(deadline);
    }

    /// Returns true if the wake event must be signaled.
    #[must_use]
    fn wake_for_timer(&mut self, deadline: Instant) -> bool {
        match self {
            PoolState::Running => false,
            PoolState::RunAgain => false,
            PoolState::Waking => false,
            &mut PoolState::Sleeping(Some(current_deadline)) if current_deadline <= deadline => {
                false
            }
            PoolState::Sleeping(_) => {
                *self = PoolState::Waking;
                true
            }
        }
    }

    fn is_referencing_ops(&self) -> bool {
        match self {
            PoolState::Running => false,
            PoolState::RunAgain => false,
            PoolState::Sleeping(_) => true,
            PoolState::Waking => true,
        }
    }
}

impl IoBackend for EpollBackend {
    fn name() -> &'static str {
        "epoll"
    }

    fn run<Fut: Future>(self: &Arc<Self>, fut: Fut) -> Fut::Output {
        let mut fut = pin!(fut);

        let waker = waker_ref(self);
        let mut cx = Context::from_waker(&waker);

        let mut to_delete: Vec<_> = Vec::new();
        let mut wakers = WakerList::default();

        let mut state = self.state.lock();
        loop {
            state.state.reset();

            // Wake timers.
            state.timers.wake_expired(&mut wakers);
            drop(state);

            wakers.wake();
            to_delete.clear();

            match fut.poll_unpin(&mut cx) {
                Poll::Ready(r) => break r,
                Poll::Pending => {}
            }

            state = self.state.lock();
            // This list is only populated while in the Sleeping state.
            assert!(state.fd_ready_to_delete.is_empty());

            if state.state.can_sleep() {
                let deadline = state.timers.next_deadline();
                state.state.sleep(deadline);
                drop(state);

                let timeout = deadline
                    .map(|deadline| {
                        let now = Instant::now();
                        (deadline.max(now) - now)
                            .as_millis()
                            .try_into()
                            .unwrap_or(i32::MAX)
                    })
                    .unwrap_or(-1);

                let mut events = [libc::epoll_event { events: 0, u64: 0 }; 8];
                let n = while_eintr(|| self.epfd.wait(&mut events, timeout))
                    .expect("epoll_wait failed unexpectedly");

                // Block unnecessary wakeups.
                let _ = self.state.lock().state.wake();

                for event in &events[..n] {
                    if event.u64 == 0 {
                        self.wake_event.try_wait();
                    } else {
                        // SAFETY: the operation context is still alive and
                        // can be dereferenced. It's possible the underlying
                        // FdReady has been dropped, but in that case the
                        // associated operation context will have been added
                        // to the fd_ready_to_delete list.
                        //
                        // Note that this is only true until state reverts
                        // to the Running state, which occurs below.
                        let op = unsafe { &*(event.u64 as usize as *const FdReadyOp) };
                        op.wake_ready(event.events, &mut wakers);
                    }
                }

                state = self.state.lock();

                // Free any FdReadyOp objects that were deleted while in the epoll_wait call.
                to_delete.append(&mut state.fd_ready_to_delete);
            }
        }
    }
}

#[derive(Debug)]
struct EpollFd(File);

impl EpollFd {
    fn new() -> Result<Self, Errno> {
        // SAFETY: epoll_create1 creates a new, uniquely owned fd.
        let epfd = unsafe {
            File::from_raw_fd(libc::epoll_create1(libc::EPOLL_CLOEXEC).syscall_result()?)
        };
        Ok(Self(epfd))
    }

    fn add(&self, fd: RawFd, events: i32, context: u64) -> Result<(), Errno> {
        let mut event = libc::epoll_event {
            events: events as u32,
            u64: context,
        };
        // SAFETY: safe to call with any fd.
        unsafe {
            libc::epoll_ctl(self.0.as_raw_fd(), libc::EPOLL_CTL_ADD, fd, &mut event)
                .syscall_result()?;
        }
        Ok(())
    }

    fn del(&self, fd: RawFd) -> Result<(), Errno> {
        // SAFETY: safe to call with any fd.
        unsafe {
            libc::epoll_ctl(
                self.0.as_raw_fd(),
                libc::EPOLL_CTL_DEL,
                fd,
                std::ptr::null_mut(),
            )
            .syscall_result()?;
        }
        Ok(())
    }

    fn wait(&self, events: &mut [libc::epoll_event], timeout: i32) -> Result<usize, Errno> {
        let maxevents = events.len() as i32;
        // SAFETY: maxevents is set appropriately to write to the events slice.
        let n = unsafe {
            libc::epoll_wait(self.0.as_raw_fd(), events.as_mut_ptr(), maxevents, timeout)
                .syscall_result()?
        };
        Ok(n as usize)
    }
}

impl ArcWake for EpollBackend {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        let wake = arc_self.state.lock().state.wake();
        if wake {
            arc_self.wake_event.signal();
        }
    }
}

#[derive(Debug)]
pub struct FdReady {
    op: Arc<FdReadyOp>,
    epoll: Arc<EpollBackend>,
    fd: RawFd,
}

#[derive(Debug)]
struct FdReadyOp {
    inner: Mutex<FdReadyInner>,
}

#[derive(Debug)]
struct FdReadyInner {
    interests: PollInterestSet,
}

impl FdReadyOp {
    fn wake_ready(&self, ep_events: u32, wakers: &mut WakerList) {
        let revents = PollEvents::from_epoll_events(ep_events);
        self.inner.lock().interests.wake_ready(revents, wakers)
    }
}

impl FdReadyDriver for EpollDriver {
    type FdReady = FdReady;

    fn new_fd_ready(&self, fd: RawFd) -> io::Result<Self::FdReady> {
        let op = Arc::new(FdReadyOp {
            inner: Mutex::new(FdReadyInner {
                interests: PollInterestSet::default(),
            }),
        });
        self.inner.epfd.add(
            fd,
            libc::EPOLLET | libc::EPOLLIN | libc::EPOLLOUT | libc::EPOLLPRI | libc::EPOLLRDHUP,
            Arc::as_ptr(&op) as usize as u64,
        )?;
        // Add reference owned by epfd, reclaimed in drop.
        let _ = Arc::into_raw(op.clone());

        Ok(FdReady {
            op,
            epoll: self.inner.clone(),
            fd,
        })
    }
}

impl Drop for FdReady {
    fn drop(&mut self) {
        // N.B. This can fail if `self.fd` was closed before `self` is
        // dropped. Since other executors don't behave this way, this might
        // be a common mistake. But it's not recoverable here (e.g. by
        // ignoring EBADF or ENOENT), since `self.fd` might be reused for
        // another file descriptor in the race window between closing the fd
        // and issuing this ioctl.
        //
        // Instead, the user of this object should ensure that `self` is
        // dropped before the associated fd is closed (e.g. by putting the
        // `FdReady` above the `File` or whatever in their struct).
        self.epoll
            .epfd
            .del(self.fd)
            .expect("epoll_ctl unexpectedly failed");

        // SAFETY: Reclaiming the reference added in new_fd_ready.
        let op = unsafe { Arc::from_raw(Arc::as_ptr(&self.op)) };
        let mut state = self.epoll.state.lock();
        if state.state.is_referencing_ops() {
            state.fd_ready_to_delete.push(op);
            if state.state.wake() {
                drop(state);
                self.epoll.wake_event.signal();
            }
        }
    }
}

impl PollFdReady for FdReady {
    fn poll_fd_ready(
        &mut self,
        cx: &mut Context<'_>,
        slot: InterestSlot,
        events: PollEvents,
    ) -> Poll<PollEvents> {
        self.op.inner.lock().interests.poll_ready(cx, slot, events)
    }

    fn clear_fd_ready(&mut self, slot: InterestSlot) {
        self.op.inner.lock().interests.clear_ready(slot)
    }
}

impl WaitDriver for EpollDriver {
    type Wait = FdWait<FdReady>;

    fn new_wait(&self, fd: RawFd, read_size: usize) -> io::Result<Self::Wait> {
        Ok(FdWait::new(fd, self.new_fd_ready(fd)?, read_size))
    }
}

impl TimerDriver for EpollDriver {
    type Timer = Timer;

    fn new_timer(&self) -> Self::Timer {
        let id = self.inner.state.lock().timers.add();
        Timer {
            epoll: self.inner.clone(),
            id,
        }
    }
}

#[derive(Debug)]
pub struct Timer {
    epoll: Arc<EpollBackend>,
    id: TimerQueueId,
}

impl Drop for Timer {
    fn drop(&mut self) {
        let _waker = self.epoll.state.lock().timers.remove(self.id);
    }
}

impl PollTimer for Timer {
    fn poll_timer(&mut self, cx: &mut Context<'_>, deadline: Option<Instant>) -> Poll<Instant> {
        let mut state = self.epoll.state.lock();
        if let Some(deadline) = deadline {
            state.timers.set_deadline(self.id, deadline);
        }
        match state.timers.poll_deadline(cx, self.id) {
            TimerResult::TimedOut(now) => Poll::Ready(now),
            TimerResult::Pending(deadline) => {
                if state.state.wake_for_timer(deadline) {
                    drop(state);
                    self.epoll.wake_event.signal();
                }
                Poll::Pending
            }
        }
    }

    fn set_deadline(&mut self, deadline: Instant) {
        let mut state = self.epoll.state.lock();
        if state.timers.set_deadline(self.id, deadline) && state.state.wake_for_timer(deadline) {
            drop(state);
            self.epoll.wake_event.signal();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::EpollPool;
    use crate::executor_tests;

    #[test]
    fn waker_works() {
        EpollPool::run_with(|_| executor_tests::waker_tests())
    }

    #[test]
    fn spawn_works() {
        executor_tests::spawn_tests(|| {
            let pool = EpollPool::new();
            (pool.driver(), move || pool.run())
        })
    }

    #[test]
    fn sleep_works() {
        EpollPool::run_with(executor_tests::sleep_tests)
    }

    #[test]
    fn wait_works() {
        EpollPool::run_with(executor_tests::wait_tests)
    }

    #[test]
    fn socket_works() {
        EpollPool::run_with(executor_tests::socket_tests)
    }
}
