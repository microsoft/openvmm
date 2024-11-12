// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! File-descriptor readiness.

use crate::any::AsAny;
use crate::interest::InterestSlot;
use crate::interest::PollEvents;
use std::io;
use std::os::unix::prelude::*;
use std::task::Context;
use std::task::Poll;

/// A trait for driving the polling of file descriptor readiness.
pub trait FdReadyDriver: Unpin {
    /// The file descriptor ready type.
    type FdReady: 'static + PollFdReady;

    /// Returns a new object for polling file descriptor readiness.
    fn new_fd_ready(&self, fd: RawFd) -> io::Result<Self::FdReady>;
}

/// A trait for polling file descriptor readiness.
pub trait PollFdReady: Unpin + Send + Sync + AsAny {
    /// Polls a file descriptor for readiness.
    fn poll_fd_ready(
        &mut self,
        cx: &mut Context<'_>,
        slot: InterestSlot,
        events: PollEvents,
    ) -> Poll<PollEvents>;

    /// Clears cached socket readiness so that the next call to
    /// `poll_socket_ready` will poll the OS again.
    ///
    /// With the epoll driver, this may not be sufficient for `poll_fd_ready` to
    /// complete again--the caller must also ensure that the kernel has seen a
    /// transition to a not-ready state (e.g. by seeing EAGAIN returned from `read`).
    fn clear_fd_ready(&mut self, slot: InterestSlot);
}
