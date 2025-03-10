// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! An executor based on kqueue.

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
use futures::task::waker_ref;
use futures::task::ArcWake;
use futures::FutureExt;
use pal::unix::while_eintr;
use pal::unix::Errno;
use pal::unix::SyscallResult;
use parking_lot::Mutex;
use std::fs::File;
use std::io;
use std::os::unix::prelude::*;
use std::pin::pin;
use std::ptr::null_mut;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

/// A single-threaded task pool backed by kqueue.
pub type KqueuePool = IoPool<KqueueBackend>;

/// A driver to spawn tasks and IO objects on [`KqueuePool`].
pub type KqueueDriver = IoDriver<KqueueBackend>;

#[derive(Debug)]
pub struct KqueueBackend {
    kqfd: KqueueFd,
    state: Mutex<KqueueState>,
}

impl Default for KqueueBackend {
    fn default() -> Self {
        let kqfd = KqueueFd::new().expect("kqueue not functional");
        Self {
            kqfd,
            state: Mutex::new(KqueueState {
                state: PoolState::Running,
                timers: TimerQueue::default(),
                fd_ready_to_delete: Vec::new(),
            }),
        }
    }
}

#[derive(Debug)]
struct KqueueState {
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

impl IoBackend for KqueueBackend {
    fn name() -> &'static str {
        "kqueue"
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

                let timeout = deadline.map(|deadline| {
                    let now = Instant::now();
                    let timeout = deadline.max(now) - now;
                    libc::timespec {
                        tv_sec: timeout.as_secs() as i64,
                        tv_nsec: timeout.subsec_nanos() as i64,
                    }
                });

                let mut events = [libc::kevent64_s {
                    ident: 0,
                    filter: 0,
                    flags: 0,
                    fflags: 0,
                    data: 0,
                    udata: 0,
                    ext: [0; 2],
                }; 8];
                let n = while_eintr(|| self.kqfd.run(&[], &mut events, timeout.as_ref()))
                    .expect("kevent64 failed unexpectedly");

                // Block unnecessary wakeups.
                let _ = self.state.lock().state.wake();

                for event in &events[..n] {
                    match event.filter {
                        libc::EVFILT_USER => {
                            // No-op.
                        }
                        libc::EVFILT_READ | libc::EVFILT_WRITE => {
                            // SAFETY: the operation context is still alive and
                            // can be dereferenced. It's possible the underlying
                            // FdReady has been dropped, but in that case the
                            // associated operation context will have been added
                            // to the fd_ready_to_delete list.
                            //
                            // Note that this is only true until state reverts
                            // to the Running state, which occurs below.
                            let op = unsafe { &*(event.udata as usize as *const FdReadyOp) };
                            let mut revents = PollEvents::EMPTY;
                            if event.filter == libc::EVFILT_READ {
                                if event.flags & libc::EV_EOF != 0 {
                                    revents |= PollEvents::RDHUP;
                                }
                                revents |= PollEvents::IN;
                            } else {
                                if event.flags & libc::EV_EOF != 0 {
                                    revents |= PollEvents::HUP;
                                }
                                revents |= PollEvents::OUT;
                            };
                            op.wake_ready(revents, &mut wakers);
                        }
                        _ => unreachable!(),
                    }
                }

                state = self.state.lock();

                // Free any FdReadyOp objects that were deleted while in the kevent64 call.
                to_delete.append(&mut state.fd_ready_to_delete);
            }
        }
    }
}

impl KqueueBackend {
    fn post_user_event(&self) {
        self.kqfd
            .run(
                &[libc::kevent64_s {
                    filter: libc::EVFILT_USER,
                    flags: libc::EV_ADD | libc::EV_ONESHOT,
                    fflags: libc::NOTE_TRIGGER,
                    ..empty_event()
                }],
                &mut [],
                Some(&zero_timespec()),
            )
            .unwrap();
    }
}

#[derive(Debug)]
struct KqueueFd(File);

fn empty_event() -> libc::kevent64_s {
    libc::kevent64_s {
        ident: 0,
        filter: 0,
        flags: 0,
        fflags: 0,
        data: 0,
        udata: 0,
        ext: [0; 2],
    }
}

fn zero_timespec() -> libc::timespec {
    libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    }
}

impl KqueueFd {
    fn new() -> Result<Self, Errno> {
        // SAFETY: kqueue creates a new, uniquely owned fd.
        let fd = unsafe { File::from_raw_fd(libc::kqueue().syscall_result()?) };
        Ok(Self(fd))
    }

    fn run(
        &self,
        changelist: &[libc::kevent64_s],
        eventlist: &mut [libc::kevent64_s],
        timeout: Option<&libc::timespec>,
    ) -> Result<usize, Errno> {
        // SAFETY: safe to call with any fd.
        let n = unsafe {
            libc::kevent64(
                self.0.as_raw_fd(),
                changelist.as_ptr(),
                changelist.len() as i32,
                eventlist.as_mut_ptr(),
                eventlist.len() as i32,
                0,
                timeout.map_or(null_mut(), |t| t),
            )
            .syscall_result()?
        };
        Ok(n as usize)
    }
}

impl ArcWake for KqueueBackend {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        let wake = arc_self.state.lock().state.wake();
        if wake {
            arc_self.post_user_event();
        }
    }
}

#[derive(Debug)]
pub struct FdReady {
    op: Arc<FdReadyOp>,
    kqueue: Arc<KqueueBackend>,
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
    fn wake_ready(&self, revents: PollEvents, wakers: &mut WakerList) {
        self.inner.lock().interests.wake_ready(revents, wakers)
    }
}

impl FdReadyDriver for KqueueDriver {
    type FdReady = FdReady;

    fn new_fd_ready(&self, fd: RawFd) -> io::Result<Self::FdReady> {
        let op = Arc::new(FdReadyOp {
            inner: Mutex::new(FdReadyInner {
                interests: PollInterestSet::default(),
            }),
        });
        let udata = Arc::as_ptr(&op) as usize as u64;
        self.inner.kqfd.run(
            &[
                libc::kevent64_s {
                    ident: fd as u64,
                    filter: libc::EVFILT_READ,
                    flags: libc::EV_ADD | libc::EV_CLEAR,
                    udata,
                    ..empty_event()
                },
                libc::kevent64_s {
                    ident: fd as u64,
                    filter: libc::EVFILT_WRITE,
                    flags: libc::EV_ADD | libc::EV_CLEAR,
                    udata,
                    ..empty_event()
                },
            ],
            &mut [],
            Some(&zero_timespec()),
        )?;
        // Add reference owned by kqfd, reclaimed in drop.
        let _ = Arc::into_raw(op.clone());

        Ok(FdReady {
            op,
            kqueue: self.inner.clone(),
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
        self.kqueue
            .kqfd
            .run(
                &[
                    libc::kevent64_s {
                        ident: self.fd as u64,
                        filter: libc::EVFILT_READ,
                        flags: libc::EV_DELETE,
                        ..empty_event()
                    },
                    libc::kevent64_s {
                        ident: self.fd as u64,
                        filter: libc::EVFILT_WRITE,
                        flags: libc::EV_DELETE,
                        ..empty_event()
                    },
                ],
                &mut [],
                Some(&zero_timespec()),
            )
            .expect("keven64 unexpectedly failed");

        // SAFETY: Reclaiming the reference added in new_fd_ready.
        let op = unsafe { Arc::from_raw(Arc::as_ptr(&self.op)) };
        let mut state = self.kqueue.state.lock();
        if state.state.is_referencing_ops() {
            state.fd_ready_to_delete.push(op);
            if state.state.wake() {
                drop(state);
                self.kqueue.post_user_event();
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

impl WaitDriver for KqueueDriver {
    type Wait = FdWait<FdReady>;

    fn new_wait(&self, fd: RawFd, read_size: usize) -> io::Result<Self::Wait> {
        Ok(FdWait::new(fd, self.new_fd_ready(fd)?, read_size))
    }
}

impl TimerDriver for KqueueDriver {
    type Timer = Timer;

    fn new_timer(&self) -> Self::Timer {
        let id = self.inner.state.lock().timers.add();
        Timer {
            kqueue: self.inner.clone(),
            id,
        }
    }
}

#[derive(Debug)]
pub struct Timer {
    kqueue: Arc<KqueueBackend>,
    id: TimerQueueId,
}

impl Drop for Timer {
    fn drop(&mut self) {
        let _waker = self.kqueue.state.lock().timers.remove(self.id);
    }
}

impl PollTimer for Timer {
    fn poll_timer(&mut self, cx: &mut Context<'_>, deadline: Option<Instant>) -> Poll<Instant> {
        let mut state = self.kqueue.state.lock();
        if let Some(deadline) = deadline {
            state.timers.set_deadline(self.id, deadline);
        }
        match state.timers.poll_deadline(cx, self.id) {
            TimerResult::TimedOut(now) => Poll::Ready(now),
            TimerResult::Pending(deadline) => {
                if state.state.wake_for_timer(deadline) {
                    drop(state);
                    self.kqueue.post_user_event();
                }
                Poll::Pending
            }
        }
    }

    fn set_deadline(&mut self, deadline: Instant) {
        let mut state = self.kqueue.state.lock();
        if state.timers.set_deadline(self.id, deadline) && state.state.wake_for_timer(deadline) {
            drop(state);
            self.kqueue.post_user_event();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::KqueuePool;
    use crate::executor_tests;

    #[test]
    fn waker_works() {
        KqueuePool::run_with(|_| executor_tests::waker_tests())
    }

    #[test]
    fn spawn_works() {
        executor_tests::spawn_tests(|| {
            let pool = KqueuePool::new();
            (pool.driver(), move || pool.run())
        })
    }

    #[test]
    fn sleep_works() {
        KqueuePool::run_with(executor_tests::sleep_tests)
    }

    #[test]
    fn wait_works() {
        KqueuePool::run_with(executor_tests::wait_tests)
    }

    #[test]
    fn socket_works() {
        KqueuePool::run_with(executor_tests::socket_tests)
    }
}
