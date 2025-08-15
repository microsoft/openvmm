// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for Windows job objects.

use std::io;
use std::mem::MaybeUninit;
use std::mem::size_of_val;
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
        // SAFETY: It is safe to initialize this C structure using MaybeUninit with zero initialization.
        let mut info = unsafe {
            let mut basic_limit_info = {
                let mut basic_limit_info =
                    MaybeUninit::<winapi::um::winnt::JOBOBJECT_BASIC_LIMIT_INFORMATION>::uninit();
                std::ptr::write_bytes(basic_limit_info.as_mut_ptr(), 0, 1);
                let mut basic_limit_info = basic_limit_info.assume_init();
                basic_limit_info.LimitFlags = winapi::um::winnt::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                basic_limit_info
            };
            let mut extended_limit_info =
                MaybeUninit::<winapi::um::winnt::JOBOBJECT_EXTENDED_LIMIT_INFORMATION>::uninit();
            std::ptr::write_bytes(extended_limit_info.as_mut_ptr(), 0, 1);
            let mut extended_limit_info = extended_limit_info.assume_init();
            extended_limit_info.BasicLimitInformation = basic_limit_info;
            extended_limit_info
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
