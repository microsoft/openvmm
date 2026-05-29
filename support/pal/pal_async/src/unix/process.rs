// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unix process wait implementations.

// UNSAFETY: Needed for the pidfd_open syscall.
#![expect(unsafe_code)]

use crate::fd::FdReadyDriver;
use crate::fd::PollFdReady;
use crate::interest::InterestSlot;
use crate::interest::PollEvents;
use crate::process::DynProcessWaitDriver;
use crate::process::PollProcessWait;
use crate::process::PolledChild;
use crate::process::ProcessWaitDriver;
use std::future::Future;
use std::future::poll_fn;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::prelude::*;
use std::task::Context;
use std::task::Poll;

/// A process wait implementation backed by pidfd readiness polling.
///
/// Caches the signaled state so that subsequent polls return immediately
/// without relying on another epoll edge.
pub struct PidFdProcessWait<F> {
    fd_ready: F,
    signaled: bool,
}

impl<F: PollFdReady> PollProcessWait for PidFdProcessWait<F> {
    fn poll_process_exit(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
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

impl<T: FdReadyDriver> ProcessWaitDriver for T {
    type ProcessWait = PidFdProcessWait<T::FdReady>;

    fn new_process_wait_pidfd(&self, pidfd: RawFd) -> io::Result<Self::ProcessWait> {
        let fd_ready = self.new_fd_ready(pidfd)?;
        Ok(PidFdProcessWait {
            fd_ready,
            signaled: false,
        })
    }
}

/// Opens a pidfd for an existing process.
fn pidfd_open(pid: i32) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open is a simple syscall that creates a new file
    // descriptor for monitoring the given pid.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0 as libc::c_int) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pidfd_open returned a valid file descriptor on success.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

// --- PolledChild<std::process::Child> construction (Linux) ---

impl PolledChild<std::process::Child> {
    /// Creates a new `PolledChild` wrapping a [`std::process::Child`].
    ///
    /// Opens a pidfd for the child process to poll for exit.
    pub fn new(
        driver: &(impl ?Sized + DynProcessWaitDriver),
        child: std::process::Child,
    ) -> io::Result<Self> {
        let pidfd = pidfd_open(child.id() as i32)?;
        let wait = driver.new_dyn_process_wait_pidfd(pidfd.as_fd().as_raw_fd())?;
        Ok(Self {
            wait: Some(wait),
            owned_pidfd: Some(pidfd),
            child,
        })
    }
}

// --- PolledChild<pal::unix::process::Child> ---

impl PolledChild<pal::unix::process::Child> {
    /// Creates a new `PolledChild` wrapping a [`pal::unix::process::Child`].
    ///
    /// Uses the child's existing pidfd to poll for exit.
    pub fn new(
        driver: &(impl ?Sized + DynProcessWaitDriver),
        child: pal::unix::process::Child,
    ) -> io::Result<Self> {
        let wait = driver.new_dyn_process_wait_pidfd(child.as_fd().as_raw_fd())?;
        Ok(Self {
            wait: Some(wait),
            owned_pidfd: None,
            child,
        })
    }

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
