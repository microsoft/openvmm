// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Wait-related functionality.

use crate::any::AsAny;
use crate::driver::Driver;
use crate::driver::PollImpl;
use std::future::poll_fn;
use std::io;
#[cfg(unix)]
use std::os::unix::prelude::*;
#[cfg(windows)]
use std::os::windows::prelude::*;
use std::task::Context;
use std::task::Poll;

/// A trait for driving kernel event (Windows events or Unix eventfd) waits.
pub trait WaitDriver: Unpin {
    /// The wait object.
    type Wait: 'static + PollWait;

    /// Creates a new wait.
    #[cfg(windows)]
    fn new_wait(&self, handle: RawHandle) -> io::Result<Self::Wait>;
    /// Creates a new wait.
    ///
    /// Signals will be consumed using reads of `read_size` bytes, with 8-byte
    /// buffer alignment. `read_size` must be at most [`MAXIMUM_WAIT_READ_SIZE`]
    /// bytes.
    #[cfg(unix)]
    fn new_wait(&self, fd: RawFd, read_size: usize) -> io::Result<Self::Wait>;
}

/// The maximum `read_size` for [`WaitDriver::new_wait`].
#[cfg(unix)]
pub const MAXIMUM_WAIT_READ_SIZE: usize = 8;

/// A trait for polling the state of waits.
pub trait PollWait: Unpin + Send + Sync + AsAny {
    /// Polls a wait for completion, consuming the object's wait signal.
    ///
    /// Depending on the wait object, this may fail. For platform events (e.g.,
    /// eventfd on Linux, and NT events on Windows), this cannot fail.
    fn poll_wait(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>>;

    /// Cancels a polled wait.
    ///
    /// Returns true if the wait signal was consumed.
    fn poll_cancel_wait(&mut self, cx: &mut Context<'_>) -> Poll<bool>;
}

impl std::fmt::Debug for dyn PollWait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad("PollWait")
    }
}

/// A polled wait object.
#[derive(Debug)]
pub struct PolledWait<T> {
    wait: PollImpl<dyn PollWait>,
    event: T,
}

#[cfg(windows)]
impl<T: AsHandle> PolledWait<T> {
    /// Returns a new polled wait object wrapping `event`.
    ///
    /// Typically `T` will be [`pal_event::Event`].
    pub fn new(driver: &(impl ?Sized + Driver), event: T) -> io::Result<Self> {
        let wait = driver.new_dyn_wait(event.as_handle().as_raw_handle())?;
        Ok(Self { wait, event })
    }
}

#[cfg(unix)]
impl<T: AsFd> PolledWait<T> {
    /// Returns a new polled wait object wrapping `event`.
    ///
    /// Typically `T` will be [`pal_event::Event`]. The read size for consuming the
    /// fd's signal will be 8 bytes to match the behavior of eventfd.
    pub fn new(driver: &(impl ?Sized + Driver), event: T) -> io::Result<Self> {
        Self::new_with_size(driver, event, 8)
    }

    /// Returns a new polled wait object wrapping `event`, with a specific sized
    /// read to consume the event.
    pub fn new_with_size(
        driver: &(impl ?Sized + Driver),
        event: T,
        read_size: usize,
    ) -> io::Result<Self> {
        let wait = driver.new_dyn_wait(event.as_fd().as_raw_fd(), read_size)?;
        Ok(Self { wait, event })
    }
}

impl<T> PolledWait<T> {
    /// Returns the inner wait object.
    ///
    /// With some drivers, this may leak a signal.
    pub fn into_inner(self) -> T {
        self.event
    }

    /// Gets a reference to the inner wait object.
    pub fn get(&self) -> &T {
        &self.event
    }

    /// Polls for the wait object to be signaled.
    pub fn poll_wait(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.wait.poll_wait(cx)
    }

    /// Waits for the wait object to be signaled.
    pub fn wait(&mut self) -> impl '_ + Unpin + Future<Output = io::Result<()>> {
        poll_fn(move |cx| self.poll_wait(cx))
    }
}
