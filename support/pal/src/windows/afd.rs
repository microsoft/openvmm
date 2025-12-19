// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(windows)]

//! Code to interact with the Windows AFD (socket) driver.

use super::SendSyncRawHandle;
use super::UnicodeString;
use super::chk_status;
use ioapiset::DeviceIoControl;
use minwinbase::OVERLAPPED;
use ntapi::ntioapi::NtOpenFile;
use ntdef::OBJECT_ATTRIBUTES;
use std::fs::File;
use std::mem::zeroed;
use std::os::windows::prelude::*;
use std::ptr::null_mut;
use winapi::shared::ntdef;
use winapi::shared::winerror;
use winapi::um::ioapiset;
use winapi::um::minwinbase;
use winapi::um::winnt;
use winerror::ERROR_IO_PENDING;

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct PollInfo {
    pub timeout: i64,
    pub number_of_handles: u32,
    pub exclusive: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct PollHandleInfo {
    pub handle: SendSyncRawHandle,
    pub events: u32,
    pub status: i32,
}

pub const POLL_RECEIVE: u32 = 1 << 0;
pub const POLL_RECEIVE_EXPEDITED: u32 = 1 << 1;
pub const POLL_SEND: u32 = 1 << 2;
pub const POLL_DISCONNECT: u32 = 1 << 3;
pub const POLL_ABORT: u32 = 1 << 4;
pub const POLL_LOCAL_CLOSE: u32 = 1 << 5;
pub const POLL_CONNECT: u32 = 1 << 6;
pub const POLL_ACCEPT: u32 = 1 << 7;
pub const POLL_CONNECT_FAIL: u32 = 1 << 8;
pub const POLL_QOS: u32 = 1 << 9;
pub const POLL_GROUP_QOS: u32 = 1 << 10;
pub const POLL_ROUTING_IF_CHANGE: u32 = 1 << 11;
pub const POLL_ADDRESS_LIST_CHANGE: u32 = 1 << 12;

const IOCTL_AFD_POLL: u32 = 0x00012024;

pub fn open_afd() -> std::io::Result<File> {
    unsafe {
        let mut pathu: UnicodeString = "\\Device\\Afd\\hvlite".try_into().expect("string fits");
        let mut oa = OBJECT_ATTRIBUTES {
            Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: null_mut(),
            ObjectName: pathu.as_mut_ptr(),
            Attributes: 0,
            SecurityDescriptor: null_mut(),
            SecurityQualityOfService: null_mut(),
        };
        let mut handle = null_mut();
        let mut iosb = zeroed();
        chk_status(NtOpenFile(
            &mut handle,
            winnt::SYNCHRONIZE,
            &mut oa,
            &mut iosb,
            0,
            0,
        ))?;
        Ok(File::from_raw_handle(handle))
    }
}

/// Issues IOCTL_AFD_POLL, returning `true` if the operation completed
/// successfully immediately and `false` if the operation is pending.
///
/// # Panics
/// Panics if the operation fails for any reason.
///
/// # Safety
/// `handle` must be a valid file handle. `poll_info` and `overlapped` must live
/// for the lifetime of the operation. `poll_info` must be valid for at least
/// `len` bytes.
pub unsafe fn poll(
    handle: RawHandle,
    poll_info: *mut PollInfo,
    len: usize,
    overlapped: *mut OVERLAPPED,
) -> bool {
    let mut returned = 0;
    let ret = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_AFD_POLL,
            poll_info.cast::<std::ffi::c_void>(),
            len as u32,
            poll_info.cast::<std::ffi::c_void>(),
            len as u32,
            &mut returned,
            overlapped,
        )
    };
    if ret != 0 {
        true
    } else {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
            panic!("afd failure: {}", error);
        }
        false
    }
}
