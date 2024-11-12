// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code for managing asynchronous waits of eventfds and similar objects.

use crate::fd::PollFdReady;
use crate::interest::InterestSlot;
use crate::interest::PollEvents;
use crate::wait::PollWait;
use crate::wait::MAXIMUM_WAIT_READ_SIZE;
use pal::unix::Errno;
use pal::unix::SyscallResult;
use std::os::unix::prelude::*;
use std::task::Context;
use std::task::Poll;

/// A [`PollWait`] implementation that waits for an fd to be signaled, then
/// reads from it.
#[derive(Debug)]
pub struct FdWait<T> {
    fd_ready: T,
    fd: RawFd,
    read_size: usize,
}

impl<T: PollFdReady> FdWait<T> {
    /// Returns a new instance that waits for `fd` to be ready via `fd_ready`,
    /// then reads `read_size` bytes from it.
    ///
    /// Panics if `read_size` is greater than [`MAXIMUM_WAIT_READ_SIZE`].
    pub fn new(fd: RawFd, fd_ready: T, read_size: usize) -> Self {
        assert!(read_size <= MAXIMUM_WAIT_READ_SIZE);
        Self {
            fd_ready,
            fd,
            read_size,
        }
    }
}

impl<T: 'static + PollFdReady> PollWait for FdWait<T> {
    fn poll_wait(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        loop {
            std::task::ready!(self
                .fd_ready
                .poll_fd_ready(cx, InterestSlot::Read, PollEvents::IN));

            self.fd_ready.clear_fd_ready(InterestSlot::Read);

            let mut buf = [0u64; 1];
            assert!(self.read_size <= size_of_val(&buf));

            // Consume the event's signal state so that we can get subsequent signals.
            //
            // SAFETY: calling with owned fd and appropriately sized buffer.
            let r = unsafe {
                libc::read(self.fd, buf.as_mut_ptr().cast(), self.read_size).syscall_result()
            };

            match r {
                Ok(_) => break,
                Err(Errno(libc::EAGAIN)) => {
                    // The event is not actually ready, presumably due to a
                    // race. Loop around again.
                }
                Err(err) => Err(err)?,
            }
        }

        Poll::Ready(Ok(()))
    }

    fn poll_cancel_wait(&mut self, _cx: &mut Context<'_>) -> Poll<bool> {
        // No need to cancel anything, since the wait signal is synchronously
        // consumed in `poll_wait`.
        Poll::Ready(false)
    }
}
