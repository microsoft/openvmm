// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Driver trait.

// UNSAFETY: Needed to define and implement the unsafe new_dyn_overlapped_file method.
#![cfg_attr(windows, allow(unsafe_code))]

#[cfg(unix)]
use crate::fd::FdReadyDriver;
#[cfg(unix)]
use crate::fd::PollFdReady;
use crate::socket::PollSocketReady;
use crate::socket::SocketReadyDriver;
#[cfg(windows)]
use crate::sys::overlapped::IoOverlapped;
#[cfg(windows)]
use crate::sys::overlapped::OverlappedIoDriver;
use crate::task::Spawn;
use crate::timer::PollTimer;
use crate::timer::TimerDriver;
use crate::wait::PollWait;
use crate::wait::WaitDriver;
use smallbox::space::S4;
use smallbox::SmallBox;
use std::io;
#[cfg(unix)]
use std::os::unix::prelude::*;
#[cfg(windows)]
use std::os::windows::prelude::*;
use std::sync::Arc;

/// A generic `Box`-like container of one of the polled types.
pub type PollImpl<T> = SmallBox<T, S4>;

/// A driver that supports polled IO.
pub trait Driver: 'static + Send + Sync {
    /// Returns a new timer.
    fn new_dyn_timer(&self) -> PollImpl<dyn PollTimer>;

    /// Returns a new object for polling file descriptor readiness.
    #[cfg(unix)]
    fn new_dyn_fd_ready(&self, fd: RawFd) -> io::Result<PollImpl<dyn PollFdReady>>;

    /// Creates a new object for polling socket readiness.
    #[cfg(windows)]
    fn new_dyn_socket_ready(&self, socket: RawSocket) -> io::Result<PollImpl<dyn PollSocketReady>>;

    /// Creates a new object for polling socket readiness.
    #[cfg(unix)]
    fn new_dyn_socket_ready(&self, socket: RawFd) -> io::Result<PollImpl<dyn PollSocketReady>>;

    /// Creates a new wait.
    #[cfg(windows)]
    fn new_dyn_wait(&self, handle: RawHandle) -> io::Result<PollImpl<dyn PollWait>>;

    /// Creates a new wait.
    ///
    /// Signals will be consumed using reads of `read_size` bytes, with 8-byte
    /// buffer alignment. `read_size` must be at most
    /// [`MAXIMUM_WAIT_READ_SIZE`](super::wait::MAXIMUM_WAIT_READ_SIZE) bytes.
    #[cfg(unix)]
    fn new_dyn_wait(&self, fd: RawFd, read_size: usize) -> io::Result<PollImpl<dyn PollWait>>;

    /// Creates a new overlapped file handler.
    ///
    /// # Safety
    /// The caller must ensure that they exclusively own `handle`, and that
    /// `handle` stays alive until the new handler is dropped.
    #[cfg(windows)]
    unsafe fn new_dyn_overlapped_file(
        &self,
        handle: RawHandle,
    ) -> io::Result<PollImpl<dyn IoOverlapped>>;
}

#[cfg(unix)]
impl<T> Driver for T
where
    T: 'static + Send + Sync + FdReadyDriver + TimerDriver + SocketReadyDriver + WaitDriver,
{
    fn new_dyn_timer(&self) -> PollImpl<dyn PollTimer> {
        smallbox::smallbox!(self.new_timer())
    }

    fn new_dyn_fd_ready(&self, fd: RawFd) -> io::Result<PollImpl<dyn PollFdReady>> {
        Ok(smallbox::smallbox!(self.new_fd_ready(fd)?))
    }

    fn new_dyn_socket_ready(&self, socket: RawFd) -> io::Result<PollImpl<dyn PollSocketReady>> {
        Ok(smallbox::smallbox!(self.new_socket_ready(socket)?))
    }

    fn new_dyn_wait(&self, fd: RawFd, read_size: usize) -> io::Result<PollImpl<dyn PollWait>> {
        Ok(smallbox::smallbox!(self.new_wait(fd, read_size)?))
    }
}

#[cfg(windows)]
impl<T> Driver for T
where
    T: 'static + Send + Sync + TimerDriver + SocketReadyDriver + WaitDriver + OverlappedIoDriver,
{
    fn new_dyn_timer(&self) -> PollImpl<dyn PollTimer> {
        smallbox::smallbox!(self.new_timer())
    }

    fn new_dyn_socket_ready(&self, socket: RawSocket) -> io::Result<PollImpl<dyn PollSocketReady>> {
        Ok(smallbox::smallbox!(self.new_socket_ready(socket)?))
    }

    fn new_dyn_wait(&self, handle: RawHandle) -> io::Result<PollImpl<dyn PollWait>> {
        Ok(smallbox::smallbox!(self.new_wait(handle)?))
    }

    unsafe fn new_dyn_overlapped_file(
        &self,
        handle: RawHandle,
    ) -> io::Result<PollImpl<dyn IoOverlapped>> {
        // SAFETY: caller guarantees contract
        Ok(smallbox::smallbox!(unsafe {
            self.new_overlapped_file(handle)
        }?))
    }
}

#[cfg(unix)]
impl Driver for Box<dyn Driver> {
    fn new_dyn_timer(&self) -> PollImpl<dyn PollTimer> {
        self.as_ref().new_dyn_timer()
    }

    fn new_dyn_fd_ready(&self, fd: RawFd) -> io::Result<PollImpl<dyn PollFdReady>> {
        self.as_ref().new_dyn_fd_ready(fd)
    }

    fn new_dyn_socket_ready(&self, socket: RawFd) -> io::Result<PollImpl<dyn PollSocketReady>> {
        self.as_ref().new_dyn_socket_ready(socket)
    }

    fn new_dyn_wait(&self, fd: RawFd, read_size: usize) -> io::Result<PollImpl<dyn PollWait>> {
        self.as_ref().new_dyn_wait(fd, read_size)
    }
}

#[cfg(windows)]
impl Driver for Box<dyn Driver> {
    fn new_dyn_timer(&self) -> PollImpl<dyn PollTimer> {
        self.as_ref().new_dyn_timer()
    }

    fn new_dyn_socket_ready(&self, socket: RawSocket) -> io::Result<PollImpl<dyn PollSocketReady>> {
        self.as_ref().new_dyn_socket_ready(socket)
    }

    fn new_dyn_wait(&self, handle: RawHandle) -> io::Result<PollImpl<dyn PollWait>> {
        self.as_ref().new_dyn_wait(handle)
    }

    unsafe fn new_dyn_overlapped_file(
        &self,
        handle: RawHandle,
    ) -> io::Result<PollImpl<dyn IoOverlapped>> {
        // SAFETY: caller guarantees contract
        unsafe { self.as_ref().new_dyn_overlapped_file(handle) }
    }
}

#[cfg(unix)]
impl Driver for Arc<dyn Driver> {
    fn new_dyn_timer(&self) -> PollImpl<dyn PollTimer> {
        self.as_ref().new_dyn_timer()
    }

    fn new_dyn_fd_ready(&self, fd: RawFd) -> io::Result<PollImpl<dyn PollFdReady>> {
        self.as_ref().new_dyn_fd_ready(fd)
    }

    fn new_dyn_socket_ready(&self, socket: RawFd) -> io::Result<PollImpl<dyn PollSocketReady>> {
        self.as_ref().new_dyn_socket_ready(socket)
    }

    fn new_dyn_wait(&self, fd: RawFd, read_size: usize) -> io::Result<PollImpl<dyn PollWait>> {
        self.as_ref().new_dyn_wait(fd, read_size)
    }
}

#[cfg(windows)]
impl Driver for Arc<dyn Driver> {
    fn new_dyn_timer(&self) -> PollImpl<dyn PollTimer> {
        self.as_ref().new_dyn_timer()
    }

    fn new_dyn_socket_ready(&self, socket: RawSocket) -> io::Result<PollImpl<dyn PollSocketReady>> {
        self.as_ref().new_dyn_socket_ready(socket)
    }

    fn new_dyn_wait(&self, handle: RawHandle) -> io::Result<PollImpl<dyn PollWait>> {
        self.as_ref().new_dyn_wait(handle)
    }

    unsafe fn new_dyn_overlapped_file(
        &self,
        handle: RawHandle,
    ) -> io::Result<PollImpl<dyn IoOverlapped>> {
        // SAFETY: caller guarantees contract
        unsafe { self.as_ref().new_dyn_overlapped_file(handle) }
    }
}

/// Trait for [`Driver`]s that also implement [`Spawn`].
pub trait SpawnDriver: Spawn + Driver {}

impl<T: Spawn + Driver> SpawnDriver for T {}
