// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Driver implementation for the `pal` crate's io-uring threadpool.

use super::threadpool::Io;
use super::threadpool::IoInitiator;
use futures::task::noop_waker_ref;
use futures::FutureExt;
use io_uring::opcode;
use io_uring::types::TimeoutFlags;
use io_uring::types::Timespec;
use pal_async::fd::FdReadyDriver;
use pal_async::fd::PollFdReady;
use pal_async::interest::InterestSlot;
use pal_async::interest::PollEvents;
use pal_async::interest::SLOT_COUNT;
use pal_async::timer::Instant;
use pal_async::timer::PollTimer;
use pal_async::timer::TimerDriver;
use pal_async::wait::PollWait;
use pal_async::wait::WaitDriver;
use pal_async::wait::MAXIMUM_WAIT_READ_SIZE;
use std::fmt::Debug;
use std::io;
use std::os::unix::prelude::*;
use std::sync::OnceLock;
use std::task::Context;
use std::task::Poll;

/// An object that can be used to initiate an IO, by returning a reference to an
/// [`IoInitiator`].
pub trait Initiate: 'static + Send + Sync + Unpin {
    /// Returns a reference to the initiator to use for IO operations.
    ///
    /// A different initiator may be returned each time this is called, allowing
    /// an object (timer, socket, etc.) to be moved between initiators.
    fn initiator(&self) -> &IoInitiator;
}

impl Initiate for IoInitiator {
    fn initiator(&self) -> &IoInitiator {
        self
    }
}

/// A [`pal_async::fd::PollFdReady`] implementation for io_uring.
#[derive(Debug)]
pub struct FdReady<T: Initiate> {
    fd: RawFd,
    initiator: T,
    interests: [Interest; SLOT_COUNT],
}

impl<T: Initiate> FdReady<T> {
    /// Creates a new `FdReady` for the given file descriptor and initiator.
    pub fn new(initiator: T, fd: RawFd) -> Self {
        FdReady {
            fd,
            initiator,
            interests: Default::default(),
        }
    }
}

impl FdReadyDriver for IoInitiator {
    type FdReady = FdReady<Self>;

    fn new_fd_ready(&self, fd: RawFd) -> io::Result<Self::FdReady> {
        Ok(FdReady::new(self.clone(), fd))
    }
}

#[derive(Debug, Default)]
struct Interest {
    io: Option<Io<()>>,
    cancelled: bool,
    events: PollEvents,
    revents: PollEvents,
}

impl<T: Initiate> PollFdReady for FdReady<T> {
    fn poll_fd_ready(
        &mut self,
        cx: &mut Context<'_>,
        slot: InterestSlot,
        events: PollEvents,
    ) -> Poll<PollEvents> {
        let interest = &mut self.interests[slot as usize];
        loop {
            if !(interest.revents & events).is_empty() {
                break Poll::Ready(interest.revents & events);
            } else if let Some(io) = &mut interest.io {
                // Cancel the current operation if not all the requested events
                // are included in the current IO.
                //
                // FUTURE: just update the current poll operation. This requires
                // >= Linux 5.11.
                if interest.events & events != events && !interest.cancelled {
                    io.cancel_poll();
                    interest.cancelled = true;
                }
                let result = std::task::ready!(io.poll_unpin(cx));
                interest.io = None;
                match result {
                    Ok(poll_revents) => {
                        interest.revents |= PollEvents::from_poll_events(poll_revents as i16);
                    }
                    Err(err) if err.raw_os_error() == Some(libc::ECANCELED) => {}
                    Err(err) => panic!("poll failed: {}", err),
                }
            } else {
                interest.events = events;
                let sqe = opcode::PollAdd::new(
                    io_uring::types::Fd(self.fd),
                    events.to_poll_events() as u32,
                )
                .build();
                // SAFETY: the PollAdd entry does not reference any external
                // memory.
                let io = unsafe { Io::new(self.initiator.initiator().clone(), sqe, ()) };
                interest.io = Some(io);
                interest.cancelled = false;
            }
        }
    }

    fn clear_fd_ready(&mut self, slot: InterestSlot) {
        let interest = &mut self.interests[slot as usize];
        interest.revents = PollEvents::EMPTY;
    }
}

/// A [`pal_async::wait::PollWait`] implementation for io_uring.
#[derive(Debug)]
pub struct FdWait<T: Initiate> {
    inner: FdWaitInner<T>,
}

#[derive(Debug)]
enum FdWaitInner<T: Initiate> {
    ViaPoll(pal_async::unix::FdWait<FdReady<T>>),
    ViaRead(FdWaitViaRead<T>),
}

impl WaitDriver for IoInitiator {
    type Wait = FdWait<Self>;

    fn new_wait(&self, fd: RawFd, read_size: usize) -> io::Result<Self::Wait> {
        Ok(FdWait::new(self.clone(), fd, read_size))
    }
}

impl<T: Initiate> FdWait<T> {
    /// Creates a new instance for the given file descriptor and initiator.
    pub fn new(initiator: T, fd: RawFd, read_size: usize) -> Self {
        static SUPPORTS_NONBLOCK_READ: OnceLock<bool> = OnceLock::new();
        // There is no easy way to detect whether the ring supports nonblocking
        // reads, but the functionality was added in the same release as linkat
        // (5.15), so that's probably as close as we're getting.
        const LINKAT: u8 = 39;
        let supports_nonblock_read =
            *SUPPORTS_NONBLOCK_READ.get_or_init(|| initiator.initiator().probe(LINKAT));

        let inner = if supports_nonblock_read {
            assert!(read_size <= MAXIMUM_WAIT_READ_SIZE);
            FdWaitInner::ViaRead(FdWaitViaRead {
                fd,
                read_size,
                initiator,
                state: FdWaitViaReadState::Idle(Box::new(0)),
            })
        } else {
            FdWaitInner::ViaPoll(pal_async::unix::FdWait::new(
                fd,
                FdReady::new(initiator, fd),
                read_size,
            ))
        };
        FdWait { inner }
    }
}

impl<T: Initiate> PollWait for FdWait<T> {
    fn poll_wait(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.inner {
            FdWaitInner::ViaPoll(wait) => wait.poll_wait(cx),
            FdWaitInner::ViaRead(wait) => wait.poll_wait(cx),
        }
    }

    fn poll_cancel_wait(&mut self, cx: &mut Context<'_>) -> Poll<bool> {
        match &mut self.inner {
            FdWaitInner::ViaPoll(wait) => wait.poll_cancel_wait(cx),
            FdWaitInner::ViaRead(wait) => wait.poll_cancel_wait(cx),
        }
    }
}

#[derive(Debug)]
struct FdWaitViaRead<T: Initiate> {
    fd: RawFd,
    read_size: usize,
    initiator: T,
    state: FdWaitViaReadState,
}

#[derive(Debug)]
enum FdWaitViaReadState {
    Idle(Box<u64>),
    ReadPending { io: Io<Box<u64>>, cancelling: bool },
    Invalid,
}

impl<T: Initiate> PollWait for FdWaitViaRead<T> {
    fn poll_wait(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            match std::mem::replace(&mut self.state, FdWaitViaReadState::Invalid) {
                FdWaitViaReadState::Idle(mut buf) => {
                    assert!(self.read_size <= 8);
                    let sqe = opcode::Read::new(
                        io_uring::types::Fd(self.fd),
                        std::ptr::from_mut(&mut *buf).cast(),
                        self.read_size as u32,
                    )
                    .build();
                    // SAFETY: the sqe's buffer is kept alive in `buf` for the
                    // lifetime of the IO.
                    let io = unsafe { Io::new(self.initiator.initiator().clone(), sqe, buf) };
                    self.state = FdWaitViaReadState::ReadPending {
                        io,
                        cancelling: false,
                    };
                }
                FdWaitViaReadState::ReadPending { mut io, cancelling } => match io.poll_unpin(cx) {
                    Poll::Ready(r) => {
                        self.state = FdWaitViaReadState::Idle(io.into_mem());
                        match r {
                            Ok(_) => break Poll::Ready(Ok(())),
                            Err(err) if err.raw_os_error() == Some(libc::ECANCELED) => {}
                            Err(err) => return Poll::Ready(Err(err)),
                        }
                    }
                    Poll::Pending => {
                        self.state = FdWaitViaReadState::ReadPending { io, cancelling };
                        return Poll::Pending;
                    }
                },
                FdWaitViaReadState::Invalid => unreachable!(),
            }
        }
    }

    fn poll_cancel_wait(&mut self, cx: &mut Context<'_>) -> Poll<bool> {
        loop {
            match std::mem::replace(&mut self.state, FdWaitViaReadState::Invalid) {
                FdWaitViaReadState::Idle(buf) => {
                    self.state = FdWaitViaReadState::Idle(buf);
                    break Poll::Ready(false);
                }
                FdWaitViaReadState::ReadPending { mut io, cancelling } => {
                    if cancelling {
                        match io.poll_unpin(cx) {
                            Poll::Ready(r) => {
                                self.state = FdWaitViaReadState::Idle(io.into_mem());
                                // If `r` is an error, it was either `ECANCELED`
                                // (so do nothing), or it was a real error. We
                                // assume that subsequent reads will return the
                                // same error, so we can ignore those here to
                                // keep the cancel contract simple for the
                                // caller.
                                break Poll::Ready(r.is_ok());
                            }
                            Poll::Pending => {
                                self.state = FdWaitViaReadState::ReadPending { io, cancelling };
                                break Poll::Pending;
                            }
                        }
                    } else {
                        io.cancel();
                        self.state = FdWaitViaReadState::ReadPending {
                            io,
                            cancelling: true,
                        };
                    }
                }
                FdWaitViaReadState::Invalid => unreachable!(),
            }
        }
    }
}

impl<T: Initiate> Drop for FdWaitViaRead<T> {
    fn drop(&mut self) {
        let _ = self.poll_cancel_wait(&mut Context::from_waker(noop_waker_ref()));
    }
}

/// A [`pal_async::timer::PollTimer`] implementation for io_uring.
#[derive(Debug)]
pub struct Timer<T: Initiate> {
    initiator: T,
    target_deadline: Instant,
    state: Option<TimerState>,
}

impl<T: Initiate> Timer<T> {
    /// Creates a new instance for the given initiator.
    pub fn new(initiator: T) -> Self {
        Timer {
            initiator,
            target_deadline: Instant::from_nanos(0),
            state: None,
        }
    }
}

#[derive(Debug)]
struct TimerState {
    io: Io<Box<Timespec>>,
    cancelled: bool,
}

impl TimerDriver for IoInitiator {
    type Timer = Timer<Self>;

    fn new_timer(&self) -> Self::Timer {
        Timer::new(self.clone())
    }
}

impl<T: Initiate> PollTimer for Timer<T> {
    fn poll_timer(&mut self, cx: &mut Context<'_>, deadline: Option<Instant>) -> Poll<Instant> {
        if let Some(deadline) = deadline {
            self.set_deadline(deadline);
        }
        loop {
            let now = Instant::now();
            if self.target_deadline <= now {
                break Poll::Ready(now);
            } else if let Some(state) = &mut self.state {
                let _ = std::task::ready!(state.io.poll_unpin(cx));
                self.state = None;
            } else {
                // Compute an absolute timeout. Note that pal's Instant is
                // CLOCK_MONOTONIC, which is exactly what io_uring supports.
                let absolute_timeout = self.target_deadline - Instant::from_nanos(0);
                let timespec = Box::new(
                    Timespec::new()
                        .sec(absolute_timeout.as_secs())
                        .nsec(absolute_timeout.subsec_nanos()),
                );
                let sqe = {
                    opcode::Timeout::new(&*timespec)
                        .flags(TimeoutFlags::ABS)
                        .build()
                };
                // SAFETY: the operation references timespec, which is boxed for
                // the duration of the IO.
                let io = unsafe { Io::new(self.initiator.initiator().clone(), sqe, timespec) };
                let state = TimerState {
                    io,
                    cancelled: false,
                };
                self.state = Some(state);
            }
        }
    }

    fn set_deadline(&mut self, deadline: Instant) {
        if let Some(state) = &mut self.state {
            // Cancel the current operation if the deadline is later than
            // the current one.
            //
            // FUTURE: just update the current operation. This requires >=
            // Linux 5.11.
            if self.target_deadline > deadline && !state.cancelled {
                state.io.cancel_timeout();
                state.cancelled = true;
            }
        }
        self.target_deadline = deadline;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::IoInitiator;
    use crate::IoUringPool;
    use futures::executor::block_on;
    use once_cell::sync::OnceCell;
    use pal_async::executor_tests;
    use pal_async::task::Spawn;
    use std::future::Future;
    use std::io;
    use std::thread::JoinHandle;

    pub struct SingleThreadPool {
        _thread: JoinHandle<()>,
        initiator: IoInitiator,
    }

    impl SingleThreadPool {
        pub fn new() -> io::Result<Self> {
            let pool = IoUringPool::new("test", 16)?;
            let initiator = pool.client().initiator().clone();
            let thread = std::thread::spawn(move || pool.run());
            Ok(Self {
                _thread: thread,
                initiator,
            })
        }

        pub fn initiator(&self) -> &IoInitiator {
            &self.initiator
        }
    }

    fn test_pool() -> io::Result<&'static SingleThreadPool> {
        // TODO: switch to std::sync::OnceLock once `get_or_try_init` is stable
        static POOL: OnceCell<SingleThreadPool> = OnceCell::new();
        POOL.get_or_try_init(SingleThreadPool::new)
    }

    macro_rules! get_pool_or_skip {
        () => {
            match test_pool() {
                Ok(pool) => pool,
                Err(err) if err.raw_os_error() == Some(libc::ENOSYS) => {
                    println!("Test case skipped (no IO-Uring support)");
                    return;
                }
                Err(err) => panic!("{}", err),
            }
        };
    }

    fn run_until<F>(pool: &SingleThreadPool, fut: F) -> F::Output
    where
        F: 'static + Future + Send,
        F::Output: Send,
    {
        block_on(pool.initiator().spawn("test", fut))
    }

    #[test]
    fn waker_works() {
        run_until(get_pool_or_skip!(), executor_tests::waker_tests());
    }

    #[test]
    fn spawn_works() {
        let pool = get_pool_or_skip!();
        executor_tests::spawn_tests(|| (pool.initiator(), || ()))
    }

    #[test]
    fn sleep_works() {
        let pool = get_pool_or_skip!();
        run_until(pool, executor_tests::sleep_tests(pool.initiator().clone()))
    }

    #[test]
    fn wait_works() {
        let pool = get_pool_or_skip!();
        run_until(pool, executor_tests::wait_tests(pool.initiator().clone()))
    }

    #[test]
    fn socket_works() {
        let pool = get_pool_or_skip!();
        run_until(pool, executor_tests::socket_tests(pool.initiator().clone()))
    }
}
