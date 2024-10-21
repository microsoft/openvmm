// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to interact with the Windows thread pool.

use std::ffi::c_void;
use std::io;
use std::os::windows::prelude::*;
use std::ptr::null_mut;
use std::ptr::NonNull;
use std::time::Duration;
use winapi::shared::minwindef::FILETIME;
use winapi::um::threadpoolapiset::CancelThreadpoolIo;
use winapi::um::threadpoolapiset::CloseThreadpoolIo;
use winapi::um::threadpoolapiset::CloseThreadpoolTimer;
use winapi::um::threadpoolapiset::CloseThreadpoolWait;
use winapi::um::threadpoolapiset::CloseThreadpoolWork;
use winapi::um::threadpoolapiset::CreateThreadpoolIo;
use winapi::um::threadpoolapiset::CreateThreadpoolTimer;
use winapi::um::threadpoolapiset::CreateThreadpoolWait;
use winapi::um::threadpoolapiset::CreateThreadpoolWork;
use winapi::um::threadpoolapiset::SetThreadpoolTimerEx;
use winapi::um::threadpoolapiset::SetThreadpoolWaitEx;
use winapi::um::threadpoolapiset::StartThreadpoolIo;
use winapi::um::threadpoolapiset::SubmitThreadpoolWork;
use winapi::um::threadpoolapiset::PTP_WIN32_IO_CALLBACK;
use winapi::um::winnt::PTP_TIMER_CALLBACK;
use winapi::um::winnt::PTP_WAIT_CALLBACK;
use winapi::um::winnt::PTP_WORK_CALLBACK;
use winapi::um::winnt::TP_IO;
use winapi::um::winnt::TP_TIMER;
use winapi::um::winnt::TP_WAIT;
use winapi::um::winnt::TP_WORK;

/// Wrapper around a threadpool wait object (TP_WAIT).
#[derive(Debug)]
pub struct TpWait(NonNull<TP_WAIT>);

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
    pub unsafe fn new(callback: PTP_WAIT_CALLBACK, context: *mut c_void) -> io::Result<Self> {
        // SAFETY: Caller ensured this is safe.
        let wait = NonNull::new(unsafe { CreateThreadpoolWait(callback, context, null_mut()) })
            .ok_or_else(io::Error::last_os_error)?;
        Ok(Self(wait))
    }

    /// Sets the handle to wait for.
    ///
    /// # Safety
    ///
    /// `handle` must be valid.
    pub unsafe fn set(&self, handle: RawHandle) {
        // SAFETY: The caller ensures this is safe when creating the object in `new`.
        unsafe {
            SetThreadpoolWaitEx(self.0.as_ptr(), handle, null_mut(), null_mut());
        }
    }

    /// Cancels the current wait. Returns true if the wait was previously
    /// active.
    pub fn cancel(&self) -> bool {
        // SAFETY: The caller ensures this is safe when creating the object in `new`.
        unsafe { SetThreadpoolWaitEx(self.0.as_ptr(), null_mut(), null_mut(), null_mut()) != 0 }
    }

    /// Retrieves a pointer to the `TP_WAIT` object.
    pub fn as_ptr(&self) -> *const TP_WAIT {
        self.0.as_ptr()
    }
}

impl Drop for TpWait {
    fn drop(&mut self) {
        // SAFETY: the object is no longer in use.
        unsafe {
            CloseThreadpoolWait(self.0.as_ptr());
        }
    }
}

/// Wrapper around a threadpool IO object (TP_IO).
#[derive(Debug)]
pub struct TpIo(NonNull<TP_IO>);

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
        let io = unsafe {
            NonNull::new(CreateThreadpoolIo(handle, callback, context, null_mut()))
                .ok_or_else(io::Error::last_os_error)?
        };
        Ok(Self(io))
    }

    /// Notifies the threadpool that an IO is being started.
    ///
    /// Failure to call this before issuing an IO will cause memory corruption.
    pub fn start_io(&self) {
        // SAFETY: The caller ensures this is safe when creating the object in `new`.
        unsafe { StartThreadpoolIo(self.0.as_ptr()) };
    }

    /// Notifies the threadpool that a started IO will not complete through the
    /// threadpool.
    ///
    /// # Safety
    /// The caller must ensure that `start_io` has been called and no associated
    /// IO will complete through the threadpool.
    pub unsafe fn cancel_io(&self) {
        // SAFETY: The caller ensures this is safe.
        unsafe { CancelThreadpoolIo(self.0.as_ptr()) };
    }
}

impl Drop for TpIo {
    fn drop(&mut self) {
        // SAFETY: the object is no longer in use.
        unsafe {
            CloseThreadpoolIo(self.0.as_ptr());
        }
    }
}

/// Wrapper around a threadpool work object (TP_WORK).
#[derive(Debug)]
pub struct TpWork(NonNull<TP_WORK>);

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
        Ok(TpWork(
            NonNull::new(unsafe { CreateThreadpoolWork(callback, context, null_mut()) })
                .ok_or_else(io::Error::last_os_error)?,
        ))
    }

    /// Submits the work item. The callback will be called for each invocation.
    pub fn submit(&self) {
        // SAFETY: The caller ensures this is safe when creating the object in `new`.
        unsafe {
            SubmitThreadpoolWork(self.0.as_ptr());
        }
    }
}

impl Drop for TpWork {
    fn drop(&mut self) {
        // SAFETY: the object is no longer in use.
        unsafe {
            CloseThreadpoolWork(self.0.as_ptr());
        }
    }
}

/// Wrapper around a threadpool timer object (TP_TIMER).
#[derive(Debug)]
pub struct TpTimer(NonNull<TP_TIMER>);

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
        let timer = NonNull::new(unsafe { CreateThreadpoolTimer(callback, context, null_mut()) })
            .ok_or_else(io::Error::last_os_error)?;
        Ok(Self(timer))
    }

    /// Starts the timer or updates the timer's timeout.
    ///
    /// Returns `true` if the timer was already set.
    pub fn set(&self, timeout: Duration) -> bool {
        let due_time_100ns = -(timeout.as_nanos() / 100).try_into().unwrap_or(i64::MAX);
        let mut due_time = FILETIME {
            dwLowDateTime: due_time_100ns as u32,
            dwHighDateTime: (due_time_100ns >> 32) as u32,
        };
        // SAFETY: The caller ensures this is safe when creating the object in `new`.
        unsafe { SetThreadpoolTimerEx(self.0.as_ptr(), &mut due_time, 0, 0) != 0 }
    }

    /// Cancels a timer.
    ///
    /// Returns `true` if the timer was previously set.
    pub fn cancel(&self) -> bool {
        // SAFETY: The caller ensures this is safe when creating the object in `new`.

        unsafe { SetThreadpoolTimerEx(self.0.as_ptr(), null_mut(), 0, 0) != 0 }
    }
}

impl Drop for TpTimer {
    fn drop(&mut self) {
        // SAFETY: The object is no longer in use.
        unsafe {
            CloseThreadpoolTimer(self.0.as_ptr());
        }
    }
}
