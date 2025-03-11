// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::chk_status;
use super::dos_to_nt_path;
use ntapi::ntioapi;
use std::ffi::c_void;
use std::fs;
use std::io;
use std::mem::zeroed;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr::null_mut;
use widestring::U16CString;
use winapi::shared::ntdef::OBJ_CASE_INSENSITIVE;
use winapi::shared::ntdef::OBJECT_ATTRIBUTES;
use winapi::um::errhandlingapi::GetLastError;
use winapi::um::minwinbase::WIN32_FIND_DATAW;

pub fn query_stat_lx_by_name(path: &Path) -> io::Result<ntioapi::FILE_STAT_LX_INFORMATION> {
    let mut pathu = dos_to_nt_path(path)?;

    let mut oa = OBJECT_ATTRIBUTES {
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
            &mut oa,
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
            handle,
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
        let handle = winapi::um::fileapi::FindFirstFileW(path.as_ptr(), &mut data);

        if handle == winapi::um::handleapi::INVALID_HANDLE_VALUE {
            Err(io::Error::from_raw_os_error(GetLastError() as i32))
        } else {
            // Close the handle opened by FindFirstfileW.
            winapi::um::fileapi::FindClose(handle);
            Ok(data)
        }
    }
}

/// Checks if the given path is a AF_UNIX socket.
pub fn is_unix_socket(path: &Path) -> io::Result<bool> {
    const IO_REPARSE_TAG_AF_UNIX: u32 = 0x80000023;

    let data = find_first_file_data(path)?;
    Ok(
        data.dwFileAttributes & winapi::um::winnt::FILE_ATTRIBUTE_REPARSE_POINT != 0
            && data.dwReserved0 == IO_REPARSE_TAG_AF_UNIX,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_stat_lx() {
        let result = query_stat_lx_by_name(r"C:\\".as_ref()).unwrap();
        unsafe { assert_ne!(&0, result.FileId.QuadPart()) };
    }
}
