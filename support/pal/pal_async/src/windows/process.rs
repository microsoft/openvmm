// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows low-level process wait implementation using waitable handles.

use crate::driver::PollImpl;
use crate::wait::PollWait;
use std::io;
use std::task::Context;
use std::task::Poll;

/// Process wait backed by a waitable process handle.
///
/// Caches the signaled state so that subsequent polls return immediately.
pub(crate) struct HandleProcessWait {
    wait: PollImpl<dyn PollWait>,
    signaled: bool,
}

impl HandleProcessWait {
    pub(crate) fn new(wait: PollImpl<dyn PollWait>) -> Self {
        Self {
            wait,
            signaled: false,
        }
    }

    pub(crate) fn poll_exit(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.signaled {
            return Poll::Ready(Ok(()));
        }
        match self.wait.poll_wait(cx) {
            Poll::Ready(result) => {
                self.signaled = true;
                Poll::Ready(result)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// The platform wait type stored inside [`PolledChild`](crate::process::PolledChild).
pub(crate) type WaitInner = HandleProcessWait;
