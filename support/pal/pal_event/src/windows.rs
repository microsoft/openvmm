// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(windows)]

use crate::Event;
use std::os::windows::prelude::*;
use windows_sys::Win32::Foundation;
use windows_sys::Win32::System::Threading;

pub type Inner = OwnedHandle;

impl Clone for Event {
    fn clone(&self) -> Self {
        Self(
            self.0
                .try_clone()
                .expect("out of resources cloning wait object"),
        )
    }
}

impl Event {
    pub(crate) fn new_inner() -> std::io::Result<Self> {
        // SAFETY: passing valid parameters as documented.
        let handle = unsafe {
            let handle = Threading::CreateEventW(
                std::ptr::null_mut(),
                false.into(),
                false.into(),
                std::ptr::null(),
            ) as RawHandle;
            if handle.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            OwnedHandle::from_raw_handle(handle)
        };
        Ok(Self(handle))
    }

    pub(crate) fn signal_inner(&self) {
        // SAFETY: passing a valid handle.
        let r = unsafe { Threading::SetEvent(self.0.as_raw_handle()) };
        if r == 0 {
            // This can only fail due to an invalid handle, which can only
            // happen due to some bug in unsafe code.
            panic!("signal failed: {}", std::io::Error::last_os_error());
        }
    }

    pub(crate) fn wait_inner(&self) {
        // SAFETY: passing a valid handle.
        let r =
            unsafe { Threading::WaitForSingleObject(self.0.as_raw_handle(), Threading::INFINITE) };
        if r != Foundation::WAIT_OBJECT_0 {
            // This can only fail due to an invalid handle, which can only
            // happen due to some bug in unsafe code.
            panic!("wait failed: {}", std::io::Error::last_os_error());
        }
    }

    pub(crate) fn try_wait_inner(&self) -> bool {
        // SAFETY: passing a valid handle.
        let r = unsafe { Threading::WaitForSingleObject(self.0.as_raw_handle(), 0) };
        match r {
            Foundation::WAIT_OBJECT_0 => true,
            Foundation::WAIT_TIMEOUT => false,
            r => {
                // This can only fail due to an invalid handle, which can only
                // happen due to some bug in unsafe code.
                panic!("wait failed ({r:#x}): {}", std::io::Error::last_os_error());
            }
        }
    }
}

impl From<OwnedHandle> for Event {
    fn from(handle: OwnedHandle) -> Self {
        Self(handle)
    }
}

impl AsHandle for Event {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.0.as_handle()
    }
}

impl From<Event> for OwnedHandle {
    fn from(event: Event) -> OwnedHandle {
        event.0
    }
}

#[cfg(feature = "mesh")]
mesh_protobuf::os_resource!(Event, OwnedHandle);
