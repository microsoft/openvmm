// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Atomic creation of files and directories with an explicit security
//! descriptor.

use super::SecurityDescriptor;
use std::io::ErrorKind;
use std::os::windows::prelude::*;
use std::path::Path;
use std::ptr::null_mut;
use widestring::U16CString;
use windows_sys::Win32::Foundation::GENERIC_READ;
use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::CREATE_NEW;
use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
use windows_sys::Win32::Storage::FileSystem::CreateFileW;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;

/// Creates a directory with `security_descriptor` applied atomically.
pub fn create_directory_with_security(
    path: &Path,
    security_descriptor: &SecurityDescriptor,
) -> std::io::Result<()> {
    let path = U16CString::from_os_str(path.as_os_str())
        .map_err(|error| std::io::Error::new(ErrorKind::InvalidInput, error))?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security_descriptor.as_ptr(),
        bInheritHandle: 0,
    };
    // SAFETY: the NUL-terminated path and security descriptor remain valid for
    // the duration of the documented Win32 call.
    if unsafe { CreateDirectoryW(path.as_ptr(), &attributes) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Exclusively creates a regular file with `security_descriptor` applied atomically.
pub fn create_file_with_security(
    path: &Path,
    security_descriptor: &SecurityDescriptor,
) -> std::io::Result<std::fs::File> {
    let path = U16CString::from_os_str(path.as_os_str())
        .map_err(|error| std::io::Error::new(ErrorKind::InvalidInput, error))?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security_descriptor.as_ptr(),
        bInheritHandle: 0,
    };
    // SAFETY: arguments satisfy CreateFileW's contracts. Ownership of a valid
    // returned handle is transferred exactly once into `File`.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `handle` is valid and uniquely owned after successful CreateFileW.
    Ok(unsafe { std::fs::File::from_raw_handle(handle) })
}
