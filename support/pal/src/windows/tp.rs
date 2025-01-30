// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to interact with the Windows thread pool.

use std::ffi::c_void;
use std::io;
use std::os::windows::prelude::*;
use std::time::Duration;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::CancelThreadpoolIo;
use windows::Win32::System::Threading::CreateThreadpoolIo;
use windows::Win32::System::Threading::CreateThreadpoolTimer;
use windows::Win32::System::Threading::CreateThreadpoolWait;
use windows::Win32::System::Threading::CreateThreadpoolWork;
use windows::Win32::System::Threading::SetThreadpoolTimerEx;
use windows::Win32::System::Threading::SetThreadpoolWaitEx;
use windows::Win32::System::Threading::StartThreadpoolIo;
use windows::Win32::System::Threading::SubmitThreadpoolWork;
use windows::Win32::System::Threading::WaitForThreadpoolWaitCallbacks;
use windows::Win32::System::Threading::PTP_IO;
use windows::Win32::System::Threading::PTP_TIMER;
use windows::Win32::System::Threading::PTP_TIMER_CALLBACK;
use windows::Win32::System::Threading::PTP_WAIT;
use windows::Win32::System::Threading::PTP_WAIT_CALLBACK;
use windows::Win32::System::Threading::PTP_WIN32_IO_CALLBACK;
use windows::Win32::System::Threading::PTP_WORK;
use windows::Win32::System::Threading::PTP_WORK_CALLBACK;
use windows_core::Free;

/// Wrapper around a threadpool wait object (TP_WAIT).
#[derive(Debug)]
pub struct TpWait(PTP_WAIT);

// SAFETY: the inner pointer is just a handle and can be safely used between
// threads.
unsafe impl Send for TpWait {}
unsafe impl Sync for TpWait {}

impl TpWait {
    /// Creates a new TP_WAIT.
    ///
    /// # Safety
    /// The caller must ensure it is safe to call `callback` with `context`
    /// whenever the wait is set and satisfied.
    pub unsafe fn new(
        callback: PTP_WAIT_CALLBACK,
        context: Option<*mut c_void>,
    ) -> io::Result<Self> {
        // SAFETY: Caller ensured this is safe.
        let wait = unsafe { CreateThreadpoolWait(callback, context, None) }?;
        Ok(Self(wait))
    }

    /// Sets the handle to wait for.
    ///
    /// # Safety
    ///
    /// `handle` must be valid.
    pub unsafe fn set_raw(&self, handle: Option<RawHandle>) -> bool {
        // SAFETY: The caller ensures this is safe when creating the object in `new`.
        unsafe { SetThreadpoolWaitEx(self.0, handle.map(HANDLE), None, None).as_bool() }
    }

    /// Sets the handle to wait for.
    pub fn set(&self, handle: Option<BorrowedHandle<'_>>) -> bool {
        // SAFETY: The caller ensures this is safe when creating the object in `new`.
        unsafe { self.set_raw(handle.map(|handle| handle.as_raw_handle())) }
    }

    /// Cancels the current wait. Returns true if the wait was previously
    /// active.
    pub fn cancel(&self) -> bool {
        // SAFETY: The caller ensures this is safe when creating the object in `new`.
        unsafe { SetThreadpoolWaitEx(self.0, None, None, None).as_bool() }
    }

    /// Retrieves a pointer to the `TP_WAIT` object.
    pub fn as_ptr(&self) -> PTP_WAIT {
        self.0
    }

    /// Waits for all callbacks to complete.
    pub fn wait_for_callbacks(&self, cancel_pending: bool) {
        // SAFETY: The caller ensures this is safe when creating the object in `new`.
        unsafe {
            WaitForThreadpoolWaitCallbacks(self.0, cancel_pending);
        }
    }
}

impl Drop for TpWait {
    fn drop(&mut self) {
        // SAFETY: the object is no longer in use.
        unsafe { self.0.free() }
    }
}

/// Wrapper around a threadpool IO object (TP_IO).
#[derive(Debug)]
pub struct TpIo(PTP_IO);

// SAFETY: the inner pointer is just a handle and can be safely used between
// threads.
unsafe impl Send for TpIo {}
unsafe impl Sync for TpIo {}

impl TpIo {
    /// Creates a new TP_IO for the file with `handle`.
    ///
    /// # Safety
    /// The caller must ensure that `handle` can be safely associated with the
    /// thread pool, and that it is safe to call `callback` with `context`
    /// whenever an IO completes.
    ///
    /// Note: once `handle` is associated, the caller must ensure that
    /// `start_io` is called each time before issuing an IO. Otherwise memory
    /// corruption will occur.
    pub unsafe fn new(
        handle: RawHandle,
        callback: PTP_WIN32_IO_CALLBACK,
        context: *mut c_void,
    ) -> io::Result<Self> {
        // SAFETY: Caller ensured this is safe.
        let io = unsafe { CreateThreadpoolIo(HANDLE(handle), callback, Some(context), None)? };
        Ok(Self(io))
    }

    /// Notifies the threadpool that an IO is being started.
    ///
    /// Failure to call this before issuing an IO will cause memory corruption.
    pub fn start_io(&self) {
        // SAFETY: The caller ensures this is safe when creating the object in `new`.
        unsafe { StartThreadpoolIo(self.0) };
    }

    /// Notifies the threadpool that a started IO will not complete through the
    /// threadpool.
    ///
    /// # Safety
    /// The caller must ensure that `start_io` has been called and no associated
    /// IO will complete through the threadpool.
    pub unsafe fn cancel_io(&self) {
        // SAFETY: The caller ensures this is safe.
        unsafe { CancelThreadpoolIo(self.0) };
    }
}

impl Drop for TpIo {
    fn drop(&mut self) {
        // SAFETY: the object is no longer in use.
        unsafe {
            self.0.free();
        }
    }
}

/// Wrapper around a threadpool work object (TP_WORK).
#[derive(Debug)]
pub struct TpWork(PTP_WORK);

// SAFETY: the inner pointer is just a handle and can be safely used between
// threads.
unsafe impl Sync for TpWork {}
unsafe impl Send for TpWork {}

impl TpWork {
    /// Creates a new threadpool work item for the file with `handle`.
    ///
    /// # Safety
    /// The caller must ensure that it is safe to call `callback` with `context`
    /// whenever the work is submitted.
    pub unsafe fn new(callback: PTP_WORK_CALLBACK, context: *mut c_void) -> io::Result<Self> {
        Ok(TpWork(unsafe {
            CreateThreadpoolWork(callback, Some(context), None)
        }?))
    }

    /// Submits the work item. The callback will be called for each invocation.
    pub fn submit(&self) {
        // SAFETY: The caller ensures this is safe when creating the object in `new`.
        unsafe {
            SubmitThreadpoolWork(self.0);
        }
    }
}

impl Drop for TpWork {
    fn drop(&mut self) {
        // SAFETY: the object is no longer in use.
        unsafe {
            self.0.free();
        }
    }
}

/// Wrapper around a threadpool timer object (TP_TIMER).
#[derive(Debug)]
pub struct TpTimer(PTP_TIMER);

// SAFETY: the inner pointer is just a handle and can be safely used between
// threads.
unsafe impl Sync for TpTimer {}
unsafe impl Send for TpTimer {}

impl TpTimer {
    /// Creates a new timer.
    ///
    /// # Safety
    /// The caller must ensure it is safe to call `callback` with `context`
    /// whenever the timer expires.
    pub unsafe fn new(callback: PTP_TIMER_CALLBACK, context: *mut c_void) -> io::Result<Self> {
        // SAFETY: Caller ensured this is safe.
        let timer = unsafe { CreateThreadpoolTimer(callback, Some(context), None) }?;
        Ok(Self(timer))
    }

    /// Starts the timer or updates the timer's timeout.
    ///
    /// Returns `true` if the timer was already set.
    pub fn set(&self, timeout: Duration) -> bool {
        let due_time_100ns = -(timeout.as_nanos() / 100).try_into().unwrap_or(i64::MAX);
        let due_time = FILETIME {
            dwLowDateTime: due_time_100ns as u32,
            dwHighDateTime: (due_time_100ns >> 32) as u32,
        };
        // SAFETY: The caller ensures this is safe when creating the object in `new`.
        unsafe {
            SetThreadpoolTimerEx(self.0, Some(std::ptr::from_ref(&due_time)), 0, None).as_bool()
        }
    }

    /// Cancels a timer.
    ///
    /// Returns `true` if the timer was previously set.
    pub fn cancel(&self) -> bool {
        // SAFETY: The caller ensures this is safe when creating the object in `new`.

        unsafe { SetThreadpoolTimerEx(self.0, None, 0, None).into() }
    }
}

impl Drop for TpTimer {
    fn drop(&mut self) {
        // SAFETY: The object is no longer in use.
        unsafe {
            self.0.free();
        }
    }
}
