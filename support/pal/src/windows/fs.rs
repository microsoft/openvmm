// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::chk_status;
use super::dos_to_nt_path;
use std::ffi::c_void;
use std::fs;
use std::io;
use std::mem::zeroed;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr::null_mut;
use widestring::U16CString;
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem as ntioapi;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::OBJ_CASE_INSENSITIVE;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FindClose;
use windows_sys::Win32::Storage::FileSystem::FindFirstFileW;
use windows_sys::Win32::Storage::FileSystem::WIN32_FIND_DATAW;

pub fn query_stat_lx_by_name(path: &Path) -> io::Result<ntioapi::FILE_STAT_LX_INFORMATION> {
    let mut pathu = dos_to_nt_path(path)?;

    let oa = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: null_mut(),
        ObjectName: pathu.as_mut_ptr(),
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: null_mut(),
        SecurityQualityOfService: null_mut(),
    };

    unsafe {
        let mut iosb = zeroed();
        let mut info: ntioapi::FILE_STAT_LX_INFORMATION = zeroed();
        let info_ptr = std::ptr::from_mut(&mut info).cast::<c_void>();
        chk_status(ntioapi::NtQueryInformationByName(
            &oa,
            &mut iosb,
            info_ptr,
            size_of_val(&info) as u32,
            ntioapi::FileStatLxInformation,
        ))?;
        Ok(info)
    }
}

pub fn query_stat_lx(file: &fs::File) -> io::Result<ntioapi::FILE_STAT_LX_INFORMATION> {
    let handle = file.as_raw_handle();
    unsafe {
        let mut iosb = zeroed();
        let mut info: ntioapi::FILE_STAT_LX_INFORMATION = zeroed();
        let info_ptr = std::ptr::from_mut(&mut info).cast::<c_void>();
        chk_status(ntioapi::NtQueryInformationFile(
            handle.cast::<c_void>(),
            &mut iosb,
            info_ptr,
            size_of_val(&info) as u32,
            ntioapi::FileStatLxInformation,
        ))?;
        Ok(info)
    }
}

/// Wrapper for Win32 FindFirstFileW which only returns the data.
fn find_first_file_data(path: &Path) -> io::Result<WIN32_FIND_DATAW> {
    let path = U16CString::from_os_str(path.as_os_str())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "nul character in string"))?;

    unsafe {
        let mut data = zeroed();
        let handle = FindFirstFileW(path.as_ptr(), &mut data);

        if handle == INVALID_HANDLE_VALUE {
            Err(io::Error::from_raw_os_error(GetLastError() as i32))
        } else {
            // Close the handle opened by FindFirstfileW.
            FindClose(handle);
            Ok(data)
        }
    }
}

/// Checks if the given path is a AF_UNIX socket.
pub fn is_unix_socket(path: &Path) -> io::Result<bool> {
    const IO_REPARSE_TAG_AF_UNIX: u32 = 0x80000023;

    let data = find_first_file_data(path)?;
    Ok(data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        && data.dwReserved0 == IO_REPARSE_TAG_AF_UNIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_stat_lx() {
        let result = query_stat_lx_by_name(r"C:\\".as_ref()).unwrap();
        assert_ne!(0, result.FileId);
    }
}
