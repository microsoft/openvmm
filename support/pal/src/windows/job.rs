// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for Windows job objects.

use std::io;
use std::mem::zeroed;
use std::os::windows::io::AsHandle;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::BorrowedHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;
use std::ptr::null;
use std::ptr::null_mut;

/// A Windows job object.
pub struct Job(OwnedHandle);

impl Job {
    /// Returns a new anonymous job object.
    pub fn new() -> io::Result<Self> {
        // SAFETY: `CreateJobObjectW` returns an owned handle or null.
        unsafe {
            let job = winapi::um::jobapi2::CreateJobObjectW(null_mut(), null());
            if job.is_null() {
                return Err(io::Error::last_os_error());
            }
            Ok(OwnedHandle::from_raw_handle(job).into())
        }
    }

    /// Sets the job to terminate all attached processes when the last handle to
    /// the job is closed.
    pub fn set_terminate_on_close(&self) -> io::Result<()> {
        // SAFETY: It is safe to initialize this C structure using `zeroed`.
        let mut info = unsafe {
            winapi::um::winnt::JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
                BasicLimitInformation: winapi::um::winnt::JOBOBJECT_BASIC_LIMIT_INFORMATION {
                    LimitFlags: winapi::um::winnt::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                    ..zeroed()
                },
                ..zeroed()
            }
        };
        // SAFETY: `SetInformationJobObject` is safe to call with a valid handle.
        let r = unsafe {
            winapi::um::jobapi2::SetInformationJobObject(
                self.0.as_raw_handle(),
                winapi::um::winnt::JobObjectExtendedLimitInformation,
                std::ptr::from_mut(&mut info).cast(),
                size_of_val(&info) as u32,
            )
        };
        if r == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Assigns a process to this job object by process ID.
    pub fn assign_process(&self, process_id: u32) -> io::Result<()> {
        // SAFETY: `OpenProcess` returns an owned handle or null.
        let process_handle = unsafe {
            winapi::um::processthreadsapi::OpenProcess(
                winapi::um::winnt::PROCESS_SET_QUOTA | winapi::um::winnt::PROCESS_TERMINATE,
                0,
                process_id,
            )
        };
        if process_handle.is_null() {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: We just created this handle successfully.
        let process_handle = unsafe { OwnedHandle::from_raw_handle(process_handle) };

        // SAFETY: `AssignProcessToJobObject` is safe to call with valid handles.
        let r = unsafe {
            winapi::um::jobapi2::AssignProcessToJobObject(
                self.0.as_raw_handle(),
                process_handle.as_raw_handle(),
            )
        };
        if r == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl AsHandle for Job {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.0.as_handle()
    }
}

impl From<OwnedHandle> for Job {
    fn from(handle: OwnedHandle) -> Self {
        Self(handle)
    }
}

impl From<Job> for OwnedHandle {
    fn from(job: Job) -> Self {
        job.0
    }
}
