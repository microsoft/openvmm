// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Process wait functionality.
//!
//! Provides async primitives for waiting on child process exit without
//! consuming the child object. The low-level [`ProcessWaitDriver`] and
//! [`PollProcessWait`] traits define a platform-specific wait source, while
//! the high-level [`PolledChild`] wrapper owns a child process and provides a
//! futures-based `wait()` method.
//!
//! [`ProcessWaitDriver`] is an extension trait, not part of [`Driver`]. On
//! Linux it is blanket-implemented for all `FdReadyDriver` types via pidfd
//! polling. On Windows it is blanket-implemented for all `WaitDriver` types.
//! Platform-specific driver implementations live in the `unix` and
//! `windows` backend modules.

use crate::driver::Driver;
use crate::driver::PollImpl;
use std::future::Future;
use std::future::poll_fn;
use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
#[cfg(unix)]
use std::os::unix::prelude::*;
#[cfg(windows)]
use std::os::windows::prelude::*;
use std::task::Context;
use std::task::Poll;

/// A trait for driving process exit waits.
///
/// This is an extension trait, not part of [`Driver`]. Not all executors
/// support process waits on all platforms.
pub trait ProcessWaitDriver: Unpin {
    /// The process wait object.
    type ProcessWait: 'static + PollProcessWait;

    /// Creates a new process wait from a waitable process handle.
    #[cfg(windows)]
    fn new_process_wait_handle(&self, handle: RawHandle) -> io::Result<Self::ProcessWait>;

    /// Creates a new process wait from a pidfd.
    #[cfg(target_os = "linux")]
    fn new_process_wait_pidfd(&self, pidfd: RawFd) -> io::Result<Self::ProcessWait>;

    /// Creates a new process wait from a process ID.
    #[cfg(target_os = "macos")]
    fn new_process_wait_pid(&self, pid: libc::pid_t) -> io::Result<Self::ProcessWait>;
}

/// A trait for polling process exit.
///
/// Implementations must not reap the child process and must not consume a
/// signal by reading from a pidfd or other process wait source. The caller
/// is responsible for obtaining the exit status through the child object's
/// native API after this poll returns [`Poll::Ready`].
pub trait PollProcessWait: Unpin + Send + Sync {
    /// Polls until the process exit wait source is signaled.
    fn poll_process_exit(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>>;
}

impl std::fmt::Debug for dyn PollProcessWait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad("PollProcessWait")
    }
}

/// A type-erased process wait implementation.
pub type ProcessWaitImpl = PollImpl<dyn PollProcessWait>;

/// Extension trait for drivers that support type-erased process waits.
///
/// A blanket implementation is provided for any `T: Driver +
/// ProcessWaitDriver`. Call sites that only hold `&dyn Driver` cannot use
/// this trait directly.
pub trait DynProcessWaitDriver: Driver {
    /// Creates a new type-erased process wait from a waitable process handle.
    #[cfg(windows)]
    fn new_dyn_process_wait_handle(&self, handle: RawHandle) -> io::Result<ProcessWaitImpl>;

    /// Creates a new type-erased process wait from a pidfd.
    #[cfg(target_os = "linux")]
    fn new_dyn_process_wait_pidfd(&self, pidfd: RawFd) -> io::Result<ProcessWaitImpl>;

    /// Creates a new type-erased process wait from a process ID.
    #[cfg(target_os = "macos")]
    fn new_dyn_process_wait_pid(&self, pid: libc::pid_t) -> io::Result<ProcessWaitImpl>;
}

impl<T: Driver + ProcessWaitDriver> DynProcessWaitDriver for T {
    #[cfg(windows)]
    fn new_dyn_process_wait_handle(&self, handle: RawHandle) -> io::Result<ProcessWaitImpl> {
        Ok(smallbox::smallbox!(self.new_process_wait_handle(handle)?))
    }

    #[cfg(target_os = "linux")]
    fn new_dyn_process_wait_pidfd(&self, pidfd: RawFd) -> io::Result<ProcessWaitImpl> {
        Ok(smallbox::smallbox!(self.new_process_wait_pidfd(pidfd)?))
    }

    #[cfg(target_os = "macos")]
    fn new_dyn_process_wait_pid(&self, pid: libc::pid_t) -> io::Result<ProcessWaitImpl> {
        Ok(smallbox::smallbox!(self.new_process_wait_pid(pid)?))
    }
}

// --- High-level PolledChild wrapper ---

/// An owned child process with an asynchronous exit wait.
///
/// The `wait` field is declared before `child` so that the backend wait
/// registration is dropped before the child's underlying handle or fd.
///
/// Platform-specific constructors are provided in the `unix` and `windows`
/// backend modules.
pub struct PolledChild<C> {
    pub(crate) wait: Option<ProcessWaitImpl>,
    #[cfg(target_os = "linux")]
    #[expect(dead_code)] // Held for drop ordering; keeps the fd alive.
    pub(crate) owned_pidfd: Option<OwnedFd>,
    pub(crate) child: C,
}

impl<C> PolledChild<C> {
    /// Returns the inner child, dropping the wait registration.
    pub fn into_inner(self) -> C {
        self.child
    }

    /// Gets a reference to the inner child.
    pub fn get(&self) -> &C {
        &self.child
    }

    /// Gets a mutable reference to the inner child.
    pub fn get_mut(&mut self) -> &mut C {
        &mut self.child
    }
}

// --- PolledChild<std::process::Child> polling ---

impl PolledChild<std::process::Child> {
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
                // Spurious readiness. Wake to retry without waiting for
                // another fd readiness edge (the signaled state is cached).
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
