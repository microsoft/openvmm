// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub mod relative;
pub mod sparse;

use super::chk_status;
use super::dos_to_nt_path;
use std::ffi::OsString;
use std::ffi::c_void;
use std::fs;
use std::io;
use std::mem::zeroed;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::AsRawHandle;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::path::Prefix;
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
use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
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

/// Returns `path` in a form that the Win32 file functions take whatever its
/// length. Without the extended-length `\\?\` prefix, `CreateDirectoryW`
/// refuses a path of 248 or more UTF-16 code units, and the other functions,
/// such as `CreateFileW` and `MoveFileExW`, one of 260 or more.
///
/// The path is resolved, as Win32 resolves it, before the prefix is added,
/// since the file system takes an extended-length path as given, and a path
/// shorter than 248 code units keeps its resolved form. Win32 also takes an NT
/// path, `\??\...`, as given, and refuses an empty path itself, so both keep
/// their form, as do verbatim and device paths.
pub fn extended_length_path(path: &Path) -> io::Result<PathBuf> {
    let raw = path.as_os_str();
    if raw.is_empty() || raw.encode_wide().take(4).eq(r"\??\".encode_utf16()) {
        return Ok(path.to_path_buf());
    }
    let path = std::path::absolute(path)?;
    if path.as_os_str().encode_wide().count() < 248 {
        return Ok(path);
    }
    let (prefix, skip) = match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(_) => (r"\\?\", 0),
            Prefix::UNC(..) => (r"\\?\UNC\", 2),
            _ => ("", 0),
        },
        _ => ("", 0),
    };
    let extended = prefix
        .encode_utf16()
        .chain(path.as_os_str().encode_wide().skip(skip))
        .collect::<Vec<_>>();
    Ok(OsString::from_wide(&extended).into())
}

/// Renames `from` to `to` in one step, unless something exists at `to`.
///
/// Unlike [`std::fs::rename`], which replaces a file at `to`, and an empty
/// directory too where the file system supports POSIX rename semantics, such
/// as NTFS, this fails with [`io::ErrorKind::AlreadyExists`] when anything
/// exists there, and leaves it as it is: it is `MoveFileExW` without
/// `MOVEFILE_REPLACE_EXISTING`, which renames a directory only within its
/// volume. Both paths may exceed the legacy Win32 `MAX_PATH` limit.
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    let encode = |path: &Path| {
        U16CString::from_os_str(extended_length_path(path)?.as_os_str())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
    };
    let (from, to) = (encode(from)?, encode(to)?);
    // SAFETY: Both paths are NUL-terminated and outlive the call, which
    // retains neither.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn test_query_stat_lx() {
        let result = query_stat_lx_by_name(r"C:\\".as_ref()).unwrap();
        assert_ne!(0, result.FileId);
    }

    #[test]
    fn extended_length_paths_prefix_resolved_long_paths() {
        let extended = |path: &str| extended_length_path(Path::new(path)).unwrap();
        let name = |length| "a".repeat(length);

        // Disk and UNC paths take the prefix from 248 UTF-16 code units, once
        // resolved.
        let short = format!(r"C:\{}", name(244));
        let long = format!(r"C:\{}", name(245));
        assert_eq!(extended(&short), Path::new(&short));
        assert_eq!(extended(&long), Path::new(&format!(r"\\?\{long}")));
        let long = format!(r"\\server\share\{}", name(233));
        assert_eq!(
            extended(&long),
            Path::new(&format!(r"\\?\UNC\{}", &long[2..]))
        );
        assert_eq!(
            extended(&format!("C:/{}/./child/../leaf", name(300))),
            Path::new(&format!(r"\\?\C:\{}\leaf", name(300)))
        );
        let relative = std::env::current_dir().unwrap().join(name(300));
        assert_eq!(extended(&name(300)), extended(relative.to_str().unwrap()));

        // Verbatim, NT, device, and empty paths keep their form.
        for path in [
            format!(r"\\?\C:\{}", name(300)),
            format!(r"\??\C:\{}", name(300)),
            format!(r"\\.\C:\{}", name(300)),
            String::new(),
        ] {
            assert_eq!(extended(&path), Path::new(&path));
        }
    }

    #[test]
    fn rename_no_replace_leaves_existing_destinations_under_long_paths() {
        let directory = tempfile::tempdir().unwrap();
        // Exceed the legacy Win32 limits of directory and file paths.
        let mut root = directory.path().to_path_buf();
        while root.as_os_str().encode_wide().count() <= 320 {
            root.push("long-path-component");
        }
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("file"), b"source").unwrap();
        fs::create_dir(root.join("empty")).unwrap();
        fs::write(root.join("file"), b"file").unwrap();

        // `std::fs::rename` would replace the empty directory.
        for destination in ["empty", "file"] {
            let error = rename_no_replace(&source, &root.join(destination)).unwrap_err();
            assert_eq!(
                error.kind(),
                io::ErrorKind::AlreadyExists,
                "{destination}: {error}"
            );
        }
        let error = rename_no_replace(&source, &root.join("a\0b")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(fs::read_dir(root.join("empty")).unwrap().next().is_none());
        assert_eq!(fs::read(root.join("file")).unwrap(), b"file");
        assert_eq!(fs::read(source.join("file")).unwrap(), b"source");

        let renamed = root.join("nested").join("renamed");
        fs::create_dir(root.join("nested")).unwrap();
        rename_no_replace(&source, &renamed).unwrap();
        assert!(!source.exists());
        assert_eq!(fs::read(renamed.join("file")).unwrap(), b"source");
    }
}
