// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Linux low-level process wait implementation using pidfd readiness.

use crate::driver::PollImpl;
use crate::fd::PollFdReady;
use crate::interest::InterestSlot;
use crate::interest::PollEvents;
use std::io;
use std::task::Context;
use std::task::Poll;

/// Process wait backed by pidfd readiness polling.
///
/// Caches the signaled state so that subsequent polls return immediately
/// without relying on another epoll edge.
pub(crate) struct FdProcessWait {
    fd_ready: PollImpl<dyn PollFdReady>,
    signaled: bool,
}

impl FdProcessWait {
    pub(crate) fn new(fd_ready: PollImpl<dyn PollFdReady>) -> Self {
        Self {
            fd_ready,
            signaled: false,
        }
    }

    pub(crate) fn poll_exit(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.signaled {
            return Poll::Ready(Ok(()));
        }
        match self
            .fd_ready
            .poll_fd_ready(cx, InterestSlot::Read, PollEvents::IN)
        {
            Poll::Ready(_) => {
                self.signaled = true;
                Poll::Ready(Ok(()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// The platform wait type stored inside [`PolledChild`](crate::process::PolledChild).
pub(crate) type WaitInner = FdProcessWait;
