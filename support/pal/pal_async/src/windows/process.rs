// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows process wait implementations.
//!
//! Implements [`ProcessWaitDriver`] for all [`WaitDriver`] types by wrapping
//! the existing waitable-handle infrastructure. Process handles are waitable
//! on Windows and become signaled when the process exits.

use crate::process::DynProcessWaitDriver;
use crate::process::PollProcessWait;
use crate::process::PolledChild;
use crate::process::ProcessWaitDriver;
use crate::process::ProcessWaitImpl;
use crate::wait::PollWait;
use crate::wait::WaitDriver;
use std::future::Future;
use std::future::poll_fn;
use std::io;
use std::os::windows::prelude::*;
use std::task::Context;
use std::task::Poll;

/// A process wait implementation backed by a waitable process handle.
///
/// Delegates to the underlying [`PollWait`] implementation and caches
/// the signaled state so that subsequent polls return immediately
/// without re-registering with the OS.
pub struct HandleProcessWait<W> {
    wait: W,
    signaled: bool,
}

impl<W: PollWait> PollProcessWait for HandleProcessWait<W> {
    fn poll_process_exit(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
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

impl<T: WaitDriver> ProcessWaitDriver for T {
    type ProcessWait = HandleProcessWait<T::Wait>;

    fn new_process_wait_handle(&self, handle: RawHandle) -> io::Result<Self::ProcessWait> {
        let wait = self.new_wait(handle)?;
        Ok(HandleProcessWait {
            wait,
            signaled: false,
        })
    }
}

// --- PolledChild<std::process::Child> construction (Windows) ---

impl PolledChild<std::process::Child> {
    /// Creates a new `PolledChild` wrapping a [`std::process::Child`].
    ///
    /// Waits on the child's process handle to detect exit.
    pub fn new(
        driver: &(impl ?Sized + DynProcessWaitDriver),
        child: std::process::Child,
    ) -> io::Result<Self> {
        let handle = child.as_handle().as_raw_handle();
        let wait = driver.new_dyn_process_wait_handle(handle)?;
        Ok(Self {
            wait: Some(wait),
            child,
        })
    }
}

// --- PolledProcess for pal::windows::Process ---

/// An owned process handle with an asynchronous exit wait.
///
/// Unlike [`PolledChild`], this type wraps a cloneable process handle
/// (not a child with cached exit status). The exit code is returned
/// as a `u32` matching the API of [`pal::windows::Process`].
///
/// The `wait` field is declared before `process` so that the backend
/// wait registration is dropped before the process handle.
pub struct PolledProcess {
    wait: Option<ProcessWaitImpl>,
    process: pal::windows::Process,
}

impl PolledProcess {
    /// Creates a new `PolledProcess` wrapping a [`pal::windows::Process`].
    ///
    /// Waits on the process handle to detect exit.
    pub fn new(
        driver: &(impl ?Sized + DynProcessWaitDriver),
        process: pal::windows::Process,
    ) -> io::Result<Self> {
        let handle = process.as_handle().as_raw_handle();
        let wait = driver.new_dyn_process_wait_handle(handle)?;
        Ok(Self {
            wait: Some(wait),
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
            match wait.poll_process_exit(cx) {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        Poll::Ready(Ok(self.process.exit_code()))
    }

    /// Waits for the process to exit, returning its exit code.
    pub fn wait(&mut self) -> impl '_ + Unpin + Future<Output = io::Result<u32>> {
        poll_fn(move |cx| self.poll_wait(cx))
    }
}
