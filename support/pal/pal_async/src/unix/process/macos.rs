// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! macOS low-level process wait implementation using kqueue `EVFILT_PROC`.

use crate::driver::PollImpl;
use std::io;
use std::task::Context;
use std::task::Poll;

/// A trait for driving process exit waits on macOS.
///
/// Kqueue `EVFILT_PROC` is a fundamentally different mechanism from fd
/// readiness or waitable handles, so macOS needs its own driver trait.
pub trait ProcessWaitDriver: Unpin {
    /// The process wait object.
    type ProcessWait: 'static + PollProcessWait;

    /// Creates a new process wait from a process ID.
    fn new_process_wait_pid(&self, pid: i32) -> io::Result<Self::ProcessWait>;
}

/// A trait for polling process exit.
///
/// Implementations must not reap the child process. The caller is responsible
/// for obtaining the exit status through the child object's native API after
/// this poll returns [`Poll::Ready`].
pub trait PollProcessWait: Unpin + Send + Sync {
    /// Polls until the process exit wait source is signaled.
    fn poll_process_exit(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>>;
}

impl std::fmt::Debug for dyn PollProcessWait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad("PollProcessWait")
    }
}

/// A [`PollProcessWait`] implementation that is never constructed.
///
/// Used by drivers that do not support process waits (e.g., `LocalDriver`).
/// The constructor returns an error, so `poll_process_exit` is unreachable.
pub enum NoProcessWait {}

impl PollProcessWait for NoProcessWait {
    fn poll_process_exit(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match *self {}
    }
}

/// A type-erased process wait implementation.
pub type ProcessWaitImpl = PollImpl<dyn PollProcessWait>;

/// The platform wait type stored inside [`PolledChild`](crate::process::PolledChild).
///
/// Wraps the type-erased [`ProcessWaitImpl`] and provides a `poll_exit`
/// method matching the interface used by other platforms.
pub(crate) struct WaitInner(ProcessWaitImpl);

impl WaitInner {
    pub(crate) fn new(inner: ProcessWaitImpl) -> Self {
        Self(inner)
    }

    pub(crate) fn poll_exit(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0.poll_process_exit(cx)
    }
}
