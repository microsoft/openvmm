// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! An executor based on epoll.

use super::epoll_uring::EPOLL_URING_STATE;
use super::epoll_uring::EPOLL_URING_TOKEN;
use super::epoll_uring::EpollIoUring;
use super::epoll_uring::EpollUringThreadState;
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
use once_cell::sync::OnceCell;
use pal::unix::Errno;
use pal::unix::SyscallResult;
use pal::unix::while_eintr;
use pal_event::Event;
use parking_lot::Mutex;
use std::fs::File;
use std::future::Future;
use std::io;
use std::os::unix::prelude::*;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

/// An single-threaded task pool backed by epoll.
pub type EpollPool = IoPool<EpollBackend>;

/// A driver to spawn tasks and IO objects on [`EpollPool`].
pub type EpollDriver = IoDriver<EpollBackend>;

/// A weak reference to an [`EpollDriver`].
pub type WeakEpollDriver = crate::io_pool::WeakIoDriver<EpollBackend>;

#[derive(Debug)]
pub struct EpollBackend {
    epfd: EpollFd,
    wake_event: Event,
    state: Mutex<EpollState>,
    io_uring: OnceCell<EpollIoUring>,
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
            io_uring: OnceCell::new(),
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

        // Set up thread-local state for on-thread io-uring access.
        // The io-uring may not be initialized yet (it's lazy), so we
        // use a null pointer initially and update it on first use.
        let uring_thread_state = EpollUringThreadState::new(std::ptr::null());

        EPOLL_URING_STATE.with(|cell| {
            cell.lend(&uring_thread_state, || {
                let mut state = self.state.lock();
                loop {
                    state.state.reset();

                    // Wake timers.
                    state.timers.wake_expired(&mut wakers);
                    drop(state);

                    // Drain completions before polling so futures see
                    // results from the previous cycle immediately.
                    // Also update the thread-local ring pointer so
                    // queue_sqe takes the fast path for on-thread callers.
                    if let Some(uring) = self.io_uring.get() {
                        // SAFETY: We are on the epoll thread and sole accessor
                        // of the ring's SQ/CQ.
                        unsafe { uring_thread_state.set_ring(uring) }
                            .process_completions(&mut wakers);
                    }

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

                        // Flush queued SQEs before sleeping. This is
                        // the single io_uring_enter() per sleep cycle,
                        // naturally batching all SQEs queued during
                        // RunAgain into one syscall.
                        if let Some(uring) = self.io_uring.get() {
                            // SAFETY: We are on the epoll thread and sole
                            // accessor of the ring's SQ/CQ.
                            unsafe { uring_thread_state.set_ring(uring) }.flush();
                        }

                        let timeout = deadline.map(|deadline| {
                            let now = Instant::now();
                            deadline.max(now) - now
                        });

                        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 8];
                        let n = while_eintr(|| self.epfd.wait(&mut events, timeout))
                            .expect("epoll_wait failed unexpectedly");

                        // Block unnecessary wakeups.
                        let _ = self.state.lock().state.wake();

                        for event in &events[..n] {
                            if event.u64 == 0 {
                                self.wake_event.try_wait();
                            } else if event.u64 == EPOLL_URING_TOKEN {
                                // Completions are drained at the top of
                                // the loop, so nothing to do here.
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
            }) // cell.lend
        }) // EPOLL_URING_STATE.with
    }
}

#[derive(Debug)]
struct EpollFd(File);

// Miri only supports epoll_wait.
static USE_EPOLL_PWAIT2: AtomicBool = AtomicBool::new(!cfg!(miri));

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

    fn wait(
        &self,
        events: &mut [libc::epoll_event],
        timeout: Option<Duration>,
    ) -> Result<usize, Errno> {
        let maxevents = events.len() as i32;
        if USE_EPOLL_PWAIT2.load(Ordering::Relaxed) {
            let timeout = timeout.map(|timeout| libc::timespec {
                tv_sec: timeout.as_secs().try_into().unwrap_or(i64::MAX),
                tv_nsec: timeout.subsec_nanos().into(),
            });
            // SAFETY: maxevents matches the writable events slice, timeout points
            // to a valid timespec (or is null), and no signal mask is supplied.
            // Use the syscall directly to avoid requiring glibc 2.35.
            let result = unsafe {
                libc::syscall(
                    libc::SYS_epoll_pwait2,
                    self.0.as_raw_fd(),
                    events.as_mut_ptr(),
                    maxevents,
                    timeout.as_ref().map_or(std::ptr::null(), std::ptr::from_ref),
                    std::ptr::null::<libc::sigset_t>(),
                    0usize,
                ) as isize
            }
            .syscall_result();
            match result {
                Ok(n) => return Ok(n as usize),
                Err(Errno(libc::ENOSYS)) => {
                    USE_EPOLL_PWAIT2.store(false, Ordering::Relaxed);
                }
                Err(err) => return Err(err),
            }
        }

        let timeout = timeout
            .map(|timeout| timeout.as_millis().try_into().unwrap_or(i32::MAX))
            .unwrap_or(-1);
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

impl crate::io_uring::IoUringDriver for EpollDriver {
    type Submitter = EpollIoUring;

    fn io_uring_submitter(&self) -> Option<&EpollIoUring> {
        self.inner
            .io_uring
            .get_or_try_init(|| {
                EpollIoUring::new(
                    self.inner.epfd.0.as_raw_fd(),
                    self.inner.wake_event.as_fd().as_raw_fd(),
                )
            })
            .ok()
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
    use super::EpollFd;
    use super::EpollPool;
    use crate::executor_tests;
    use pal_event::Event;
    use std::os::unix::prelude::*;
    use std::time::Duration;
    use test_with_tracing::test;

    #[test]
    #[cfg(not(miri))]
    fn submillisecond_timeout() {
        let epfd = EpollFd::new().unwrap();
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 1];
        let timeout = Duration::from_micros(250);
        let start = std::time::Instant::now();
        assert_eq!(epfd.wait(&mut events, Some(timeout)).unwrap(), 0);
        let elapsed = start.elapsed();
        if super::USE_EPOLL_PWAIT2.load(std::sync::atomic::Ordering::Relaxed) {
            assert!(elapsed >= timeout);
        } else {
            println!("Submillisecond timeout assertion skipped (no epoll_pwait2 support)");
        }
    }

    #[test]
    fn wait_timeout_modes() {
        let epfd = EpollFd::new().unwrap();
        let event = Event::new();
        epfd.add(event.as_fd().as_raw_fd(), libc::EPOLLIN, 42)
            .unwrap();
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 1];
        assert_eq!(epfd.wait(&mut events, Some(Duration::ZERO)).unwrap(), 0);
        event.signal();
        assert_eq!(epfd.wait(&mut events, None).unwrap(), 1);
        let context = events[0].u64;
        assert_eq!(context, 42);
    }

    #[cfg(not(miri))]
    fn deny_epoll_pwait2(errno: i32) {
        use pal::unix::SyscallResult;

        let filter = [
            libc::sock_filter {
                code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
                jt: 0,
                jf: 0,
                k: 0, // seccomp_data.nr.
            },
            libc::sock_filter {
                code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
                jt: 0,
                jf: 1,
                k: libc::SYS_epoll_pwait2 as u32,
            },
            libc::sock_filter {
                code: (libc::BPF_RET | libc::BPF_K) as u16,
                jt: 0,
                jf: 0,
                k: libc::SECCOMP_RET_ERRNO | errno as u32,
            },
            libc::sock_filter {
                code: (libc::BPF_RET | libc::BPF_K) as u16,
                jt: 0,
                jf: 0,
                k: libc::SECCOMP_RET_ALLOW,
            },
        ];
        let program = libc::sock_fprog {
            len: filter.len() as u16,
            filter: filter.as_ptr().cast_mut(),
        };
        // SAFETY: Install a valid filter on this child thread only, allowing all
        // syscalls except epoll_pwait2, which returns the selected errno.
        unsafe {
            libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)
                .syscall_result()
                .unwrap();
            libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program)
                .syscall_result()
                .unwrap();
        }
    }

    #[test]
    #[cfg(not(miri))]
    fn epoll_pwait2_fallback() {
        use pal::unix::Errno;

        const CHILD_ERRNO: &str = "PAL_ASYNC_TEST_EPOLL_PWAIT2_ERRNO";
        if let Some(errno) = std::env::var_os(CHILD_ERRNO) {
            let errno: i32 = errno.to_str().unwrap().parse().unwrap();
            deny_epoll_pwait2(errno);
            let epfd = EpollFd::new().unwrap();
            let mut events = [libc::epoll_event { events: 0, u64: 0 }; 1];
            if errno == libc::ENOSYS {
                assert_eq!(epfd.wait(&mut events, Some(Duration::ZERO)).unwrap(), 0);
                // A subsequent probe would now return EPERM, not ENOSYS.
                deny_epoll_pwait2(libc::EPERM);
                wait_timeout_modes();
                EpollPool::run_with(executor_tests::sleep_tests);
            } else {
                for _ in 0..2 {
                    assert_eq!(
                        epfd.wait(&mut events, Some(Duration::ZERO)),
                        Err(Errno(errno))
                    );
                }
            }
            return;
        }

        for errno in [libc::ENOSYS, libc::EPERM] {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "sys::epoll::tests::epoll_pwait2_fallback",
                    "--nocapture",
                ])
                .env(CHILD_ERRNO, errno.to_string())
                .status()
                .unwrap();
            assert!(status.success(), "epoll_pwait2 errno {errno}: {status}");
        }
    }

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

    #[test]
    fn process_works() {
        EpollPool::run_with(executor_tests::process_tests)
    }

    #[test]
    fn uring_works() {
        EpollPool::run_with(executor_tests::io_uring_tests::uring_tests)
    }
}
