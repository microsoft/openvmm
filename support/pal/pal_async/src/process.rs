// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Process wait functionality.
//!
//! Provides async primitives for waiting on child process exit without
//! consuming the child object. On Linux and Windows, [`PolledChild`]
//! constructs the wait directly from existing [`Driver`]
//! primitives (fd readiness and waitable handles). On macOS,
//! `Driver::new_dyn_process_wait` dispatches to the kqueue `EVFILT_PROC`
//! mechanism via the `ProcessWaitDriver` trait.

use crate::driver::Driver;
use std::future::Future;
use std::future::poll_fn;
use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
use std::task::Context;
use std::task::Poll;
use std::task::ready;

/// macOS-specific process wait types.
#[cfg(target_os = "macos")]
pub mod macos {
    pub use crate::sys::process::macos::NoProcessWait;
    pub use crate::sys::process::macos::PollProcessWait;
    pub use crate::sys::process::macos::ProcessWaitDriver;
    pub use crate::sys::process::macos::ProcessWaitImpl;
}

/// An owned child process with an asynchronous exit wait.
///
/// The wait field is declared before `child` so that the backend wait
/// registration is dropped before the child's underlying handle or fd.
pub struct PolledChild<C> {
    wait: Option<crate::sys::process::WaitInner>,
    // Drop order: after `wait`, which may have a RawFd copy of this.
    #[cfg(target_os = "linux")]
    _owned_pidfd: Option<OwnedFd>,
    child: C,
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

/// Polls the wait backend for the exit notification, then reaps the child.
///
/// `reap` performs a *blocking* wait. On Linux (pidfd) and Windows (handle)
/// the backend only signals once the child is reapable, so the wait returns
/// immediately. On macOS the kqueue `NOTE_EXIT` notification can arrive a few
/// microseconds before `waitpid` can reap the child, so the wait blocks for
/// that brief, kernel-bounded window (XNU delivers `NOTE_EXIT` while the
/// process is exiting, with only a few non-blocking teardown steps remaining)
/// rather than spinning on a non-blocking `try_wait`.
///
/// This is the shared implementation of `PolledChild::poll_wait` for all
/// child-process types.
fn poll_child_exit(
    cx: &mut Context<'_>,
    wait: &mut Option<crate::sys::process::WaitInner>,
    reap: impl FnOnce() -> io::Result<std::process::ExitStatus>,
) -> Poll<io::Result<std::process::ExitStatus>> {
    if let Some(w) = wait {
        ready!(w.poll_exit(cx))?;
    }
    Poll::Ready(reap())
}

// --- std::process::Child ---

impl PolledChild<std::process::Child> {
    /// Creates a new `PolledChild` wrapping a [`std::process::Child`].
    pub fn new(driver: &(impl ?Sized + Driver), child: std::process::Child) -> io::Result<Self> {
        Self::new_inner(driver, child)
    }

    /// Polls for the child process to exit.
    pub fn poll_wait(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<std::process::ExitStatus>> {
        poll_child_exit(cx, &mut self.wait, || self.child.wait())
    }

    /// Waits for the child process to exit.
    pub fn wait(
        &mut self,
    ) -> impl '_ + Unpin + Future<Output = io::Result<std::process::ExitStatus>> {
        poll_fn(move |cx| self.poll_wait(cx))
    }
}

// --- pal::unix::process::Child ---

#[cfg(unix)]
impl PolledChild<pal::unix::process::Child> {
    /// Creates a new `PolledChild` wrapping a [`pal::unix::process::Child`].
    pub fn new(
        driver: &(impl ?Sized + Driver),
        child: pal::unix::process::Child,
    ) -> io::Result<Self> {
        Self::new_inner(driver, child)
    }

    /// Polls for the child process to exit.
    pub fn poll_wait(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<std::process::ExitStatus>> {
        poll_child_exit(cx, &mut self.wait, || self.child.wait())
    }

    /// Waits for the child process to exit.
    pub fn wait(
        &mut self,
    ) -> impl '_ + Unpin + Future<Output = io::Result<std::process::ExitStatus>> {
        poll_fn(move |cx| self.poll_wait(cx))
    }
}

// --- pal::windows::Process ---

/// An owned process handle with an asynchronous exit wait.
///
/// Unlike [`PolledChild`], this type wraps a cloneable process handle
/// (not a child with cached exit status). The exit code is returned
/// as a `u32` matching the API of [`pal::windows::Process`].
///
/// The `wait` field is declared before `process` so that the backend
/// wait registration is dropped before the process handle.
#[cfg(windows)]
pub struct PolledProcess {
    wait: Option<crate::sys::process::WaitInner>,
    process: pal::windows::Process,
}

#[cfg(windows)]
impl PolledProcess {
    /// Creates a new `PolledProcess` wrapping a [`pal::windows::Process`].
    ///
    /// Waits on the process handle to detect exit.
    pub fn new(
        driver: &(impl ?Sized + Driver),
        process: pal::windows::Process,
    ) -> io::Result<Self> {
        use crate::sys::process::HandleProcessWait;
        use std::os::windows::prelude::*;

        let handle = process.as_handle().as_raw_handle();
        let wait = driver.new_dyn_wait(handle)?;
        Ok(Self {
            wait: Some(HandleProcessWait::new(wait)),
            process,
        })
    }

    /// Returns the inner process, dropping the wait registration.
    pub fn into_inner(self) -> pal::windows::Process {
        self.process
    }

    /// Gets a reference to the inner process.
    pub fn get(&self) -> &pal::windows::Process {
        &self.process
    }

    /// Polls for the process to exit, returning its exit code.
    pub fn poll_wait(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<u32>> {
        if let Some(wait) = &mut self.wait {
            ready!(wait.poll_exit(cx))?;
        }
        Poll::Ready(Ok(self.process.exit_code()))
    }

    /// Waits for the process to exit, returning its exit code.
    pub fn wait(&mut self) -> impl '_ + Unpin + Future<Output = io::Result<u32>> {
        poll_fn(move |cx| self.poll_wait(cx))
    }
}

impl<C> PolledChild<C> {
    #[cfg_attr(windows, expect(dead_code))]
    /// Creates a `PolledChild` for an already-exited child.
    fn exited(child: C) -> Self {
        Self {
            wait: None,
            #[cfg(target_os = "linux")]
            _owned_pidfd: None,
            child,
        }
    }
}

/// Linux: open a pidfd and poll fd readiness.
#[cfg(target_os = "linux")]
mod linux {
    // UNSAFETY: Needed for the pidfd_open syscall.
    #![expect(unsafe_code)]

    use super::*;
    use crate::sys::process::linux::FdProcessWait;
    use std::os::unix::prelude::*;

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

    impl PolledChild<std::process::Child> {
        pub(super) fn new_inner(
            driver: &(impl ?Sized + Driver),
            mut child: std::process::Child,
        ) -> io::Result<Self> {
            // If the caller already reaped the child, don't try to register
            // notifications on its pid.
            if child.try_wait()?.is_some() {
                return Ok(Self::exited(child));
            }
            let pidfd = pidfd_open(child.id() as i32)?;
            let fd_ready = driver.new_dyn_fd_ready(pidfd.as_fd().as_raw_fd())?;
            Ok(Self {
                wait: Some(FdProcessWait::new(fd_ready)),
                _owned_pidfd: Some(pidfd),
                child,
            })
        }
    }

    impl PolledChild<pal::unix::process::Child> {
        pub(super) fn new_inner(
            driver: &(impl ?Sized + Driver),
            child: pal::unix::process::Child,
        ) -> io::Result<Self> {
            let fd_ready = driver.new_dyn_fd_ready(child.as_fd().as_raw_fd())?;
            Ok(Self {
                wait: Some(FdProcessWait::new(fd_ready)),
                _owned_pidfd: None,
                child,
            })
        }
    }
}

/// macOS: use kqueue EVFILT_PROC via `Driver::new_dyn_process_wait`.
#[cfg(target_os = "macos")]
mod macos_impl {
    use super::*;
    use crate::sys::process::WaitInner;

    impl PolledChild<std::process::Child> {
        pub(super) fn new_inner(
            driver: &(impl ?Sized + Driver),
            mut child: std::process::Child,
        ) -> io::Result<Self> {
            // If the caller already reaped the child, don't try to register
            // notifications on its pid.
            if child.try_wait()?.is_some() {
                return Ok(Self::exited(child));
            }
            let wait = driver.new_dyn_process_wait(child.id() as i32)?;
            Ok(Self {
                wait: Some(WaitInner::new(wait)),
                child,
            })
        }
    }

    impl PolledChild<pal::unix::process::Child> {
        pub(super) fn new_inner(
            driver: &(impl ?Sized + Driver),
            mut child: pal::unix::process::Child,
        ) -> io::Result<Self> {
            // If the caller already reaped the child, don't try to register
            // notifications on its pid.
            if child.try_wait()?.is_some() {
                return Ok(Self::exited(child));
            }
            let wait = driver.new_dyn_process_wait(child.id())?;
            Ok(Self {
                wait: Some(WaitInner::new(wait)),
                child,
            })
        }
    }
}

/// Windows: wait on process handle via `Driver::new_dyn_wait`.
#[cfg(windows)]
mod windows {
    use super::*;
    use crate::sys::process::HandleProcessWait;
    use std::os::windows::prelude::*;

    impl PolledChild<std::process::Child> {
        pub(super) fn new_inner(
            driver: &(impl ?Sized + Driver),
            child: std::process::Child,
        ) -> io::Result<Self> {
            let handle = child.as_handle().as_raw_handle();
            let wait = driver.new_dyn_wait(handle)?;
            Ok(Self {
                wait: Some(HandleProcessWait::new(wait)),
                child,
            })
        }
    }
}
