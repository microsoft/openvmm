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

/// Polls the wait backend for exit, then reaps via `try_wait`.
///
/// Backend readiness does not universally imply reapability:
/// - Linux (pidfd) and Windows (handle) report ready only once reapable.
/// - macOS (kqueue `NOTE_EXIT`) can fire *before* the child is reapable via
///   `waitpid`, so `try_wait` briefly returns `None`.
///
/// Backends latch their one-shot exit event, so on that race we re-poll to
/// completion rather than panicking.
fn poll_child_exit(
    cx: &mut Context<'_>,
    mut poll_exit: impl FnMut(&mut Context<'_>) -> Poll<io::Result<()>>,
    mut try_wait: impl FnMut() -> io::Result<Option<std::process::ExitStatus>>,
) -> Poll<io::Result<std::process::ExitStatus>> {
    ready!(poll_exit(cx))?;
    match try_wait()? {
        Some(status) => Poll::Ready(Ok(status)),
        // macOS: signaled but not yet reapable; re-poll (the backend stays latched).
        None => {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

/// Generates the `poll_wait`/`wait` surface for a concrete child type.
///
/// `PolledChild<C>` is generic, but its two child types --
/// [`std::process::Child`] and `pal::unix::process::Child` -- share only a
/// compatible inherent `try_wait`, not a common trait, so the (otherwise
/// identical) wait logic is stamped out per type rather than written
/// generically.
macro_rules! impl_polled_child_wait {
    ($child:ty) => {
        impl PolledChild<$child> {
            /// Polls for the child process to exit.
            pub fn poll_wait(
                &mut self,
                cx: &mut Context<'_>,
            ) -> Poll<io::Result<std::process::ExitStatus>> {
                let wait = &mut self.wait;
                let child = &mut self.child;
                poll_child_exit(
                    cx,
                    |cx| match wait {
                        Some(w) => w.poll_exit(cx),
                        None => Poll::Ready(Ok(())),
                    },
                    || child.try_wait(),
                )
            }

            /// Waits for the child process to exit.
            pub fn wait(
                &mut self,
            ) -> impl '_ + Unpin + Future<Output = io::Result<std::process::ExitStatus>> {
                poll_fn(move |cx| self.poll_wait(cx))
            }
        }
    };
}

// --- std::process::Child ---

impl PolledChild<std::process::Child> {
    /// Creates a new `PolledChild` wrapping a [`std::process::Child`].
    pub fn new(driver: &(impl ?Sized + Driver), child: std::process::Child) -> io::Result<Self> {
        Self::new_inner(driver, child)
    }
}

impl_polled_child_wait!(std::process::Child);

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
}

#[cfg(unix)]
impl_polled_child_wait!(pal::unix::process::Child);

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::task::Wake;
    use std::task::Waker;

    struct CountingWaker(AtomicUsize);

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A successful `ExitStatus` without spawning a process: `from_raw(0)` is a
    /// zero wait-status on unix and exit code 0 on windows.
    fn success_status() -> std::process::ExitStatus {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        #[cfg(windows)]
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(0)
    }

    /// macOS `NOTE_EXIT` can fire before `waitpid` can reap, so `try_wait`
    /// transiently returns `None`. The shared poll must re-poll (Pending +
    /// wake) rather than panic, then complete once the reap succeeds.
    #[test]
    fn macos_reap_race_repolls_then_completes() {
        let waker = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let task_waker: Waker = waker.clone().into();
        let mut cx = Context::from_waker(&task_waker);

        let mut poll_exit = |_: &mut Context<'_>| Poll::Ready(Ok(()));
        let mut try_calls = 0;
        let mut try_wait = || {
            try_calls += 1;
            Ok((try_calls > 1).then(success_status))
        };

        // First poll: signaled but not yet reapable -> Pending + wake.
        assert!(poll_child_exit(&mut cx, &mut poll_exit, &mut try_wait).is_pending());
        assert_eq!(waker.0.load(Ordering::Relaxed), 1);

        // Second poll: reap now succeeds.
        match poll_child_exit(&mut cx, &mut poll_exit, &mut try_wait) {
            Poll::Ready(Ok(status)) => assert!(status.success()),
            other => panic!("expected ready success, got {other:?}"),
        }
    }

    /// Linux/Windows: ready implies reapable, so this completes on the first
    /// poll with no self-wake.
    #[test]
    fn ready_and_reapable_completes_without_repoll() {
        let waker = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let task_waker: Waker = waker.clone().into();
        let mut cx = Context::from_waker(&task_waker);

        let mut poll_exit = |_: &mut Context<'_>| Poll::Ready(Ok(()));
        let mut try_wait = || Ok(Some(success_status()));

        match poll_child_exit(&mut cx, &mut poll_exit, &mut try_wait) {
            Poll::Ready(Ok(status)) => assert!(status.success()),
            other => panic!("expected ready success, got {other:?}"),
        }
        assert_eq!(
            waker.0.load(Ordering::Relaxed),
            0,
            "a reapable child must not trigger a re-poll"
        );
    }

    /// Until the backend signals exit, poll stays Pending: no reap is attempted
    /// and the task is not self-woken (the backend owns the wakeup).
    #[test]
    fn pending_before_exit_does_not_reap() {
        let waker = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let task_waker: Waker = waker.clone().into();
        let mut cx = Context::from_waker(&task_waker);

        let mut poll_exit = |_: &mut Context<'_>| Poll::Pending;
        let try_calls = AtomicUsize::new(0);
        let mut try_wait = || {
            try_calls.fetch_add(1, Ordering::Relaxed);
            Ok(Some(success_status()))
        };

        assert!(poll_child_exit(&mut cx, &mut poll_exit, &mut try_wait).is_pending());
        assert_eq!(
            try_calls.load(Ordering::Relaxed),
            0,
            "must not reap before the backend signals exit"
        );
        assert_eq!(
            waker.0.load(Ordering::Relaxed),
            0,
            "the backend owns the wakeup while pending"
        );
    }
}
