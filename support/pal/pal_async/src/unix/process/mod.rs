// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unix process wait implementations.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

use crate::process::PolledChild;
use std::future::Future;
use std::future::poll_fn;
use std::io;
use std::task::Context;
use std::task::Poll;

impl PolledChild<pal::unix::process::Child> {
    /// Polls for the child process to exit.
    pub fn poll_wait(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<std::process::ExitStatus>> {
        if let Some(wait) = &mut self.wait {
            match wait.poll_process_exit(cx) {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        match self.child.try_wait() {
            Ok(Some(status)) => Poll::Ready(Ok(status)),
            Ok(None) => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    /// Waits for the child process to exit.
    pub fn wait(
        &mut self,
    ) -> impl '_ + Unpin + Future<Output = io::Result<std::process::ExitStatus>> {
        poll_fn(move |cx| self.poll_wait(cx))
    }
}
