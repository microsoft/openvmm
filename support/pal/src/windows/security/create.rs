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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::security::LocalSecurityDescriptor;
    use std::ffi::OsString;
    use std::fs;
    use std::fs::OpenOptions;
    use std::io;
    use std::io::Read;
    use std::io::Seek;
    use std::io::Write;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;
    use test_with_tracing::test;
    use widestring::U16CStr;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
    use windows_sys::Win32::Security::Authorization::SDDL_REVISION_1;
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::GetFileSecurityW;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            loop {
                let mut id = [0; 16];
                getrandom::fill(&mut id).unwrap();
                let path = std::env::temp_dir().join(format!(
                    "pal-secure-create-{:032x}",
                    u128::from_ne_bytes(id)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                    Err(error) => panic!("failed to create test directory: {error}"),
                }
            }
        }

        fn join(&self, path: impl AsRef<Path>) -> PathBuf {
            self.0.join(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn protected_descriptor() -> LocalSecurityDescriptor {
        "D:P(A;;FA;;;WD)".parse().unwrap()
    }

    fn dacl_sddl(path: &Path) -> io::Result<String> {
        let path = U16CString::from_os_str(path.as_os_str())
            .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error))?;
        let mut required = 0;
        unsafe {
            GetFileSecurityW(
                path.as_ptr(),
                DACL_SECURITY_INFORMATION,
                null_mut(),
                0,
                &mut required,
            );
        }
        if required == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut descriptor = vec![0_u8; required as usize];
        if unsafe {
            GetFileSecurityW(
                path.as_ptr(),
                DACL_SECURITY_INFORMATION,
                descriptor.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }

        let mut sddl = null_mut();
        if unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor.as_ptr().cast_mut().cast(),
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut sddl,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let sddl_string = unsafe { U16CStr::from_ptr_str(sddl).to_string_lossy() };
        unsafe {
            LocalFree(sddl.cast());
        }
        Ok(sddl_string)
    }

    #[test]
    fn applies_protected_dacl_to_files_and_directories() {
        let root = TestDirectory::new();
        let descriptor = protected_descriptor();
        let expected = descriptor.to_sddl().unwrap();
        let directory = root.join("directory");
        let file = root.join("file");

        create_directory_with_security(&directory, &descriptor).unwrap();
        create_file_with_security(&file, &descriptor).unwrap();

        assert_eq!(dacl_sddl(&directory).unwrap(), expected);
        assert_eq!(dacl_sddl(&file).unwrap(), expected);
    }

    #[test]
    fn existing_objects_are_not_modified() {
        let root = TestDirectory::new();
        let descriptor = protected_descriptor();
        let replacement: LocalSecurityDescriptor = "D:P(A;;FRFW;;;WD)".parse().unwrap();
        let directory = root.join("directory");
        let marker = directory.join("marker");
        let file = root.join("file");

        create_directory_with_security(&directory, &descriptor).unwrap();
        fs::write(&marker, b"directory contents").unwrap();
        fs::write(&file, b"existing file contents").unwrap();
        let directory_dacl = dacl_sddl(&directory).unwrap();
        let file_dacl = dacl_sddl(&file).unwrap();

        let directory_error = create_directory_with_security(&directory, &replacement).unwrap_err();
        let file_error = create_file_with_security(&file, &replacement).unwrap_err();

        assert_eq!(directory_error.kind(), ErrorKind::AlreadyExists);
        assert_eq!(file_error.kind(), ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&marker).unwrap(), b"directory contents");
        assert_eq!(fs::read(&file).unwrap(), b"existing file contents");
        assert_eq!(dacl_sddl(&directory).unwrap(), directory_dacl);
        assert_eq!(dacl_sddl(&file).unwrap(), file_dacl);
    }

    #[test]
    fn created_file_is_read_write_and_fully_shared() {
        let root = TestDirectory::new();
        let descriptor = protected_descriptor();
        let path = root.join("file");
        let renamed = root.join("renamed");
        let mut file = create_file_with_security(&path, &descriptor).unwrap();

        file.write_all(b"contents").unwrap();
        file.rewind().unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "contents");

        let reader = OpenOptions::new().read(true).open(&path).unwrap();
        let writer = OpenOptions::new().write(true).open(&path).unwrap();
        fs::rename(&path, &renamed).unwrap();
        drop((reader, writer));

        file.write_all(b" appended").unwrap();
        drop(file);
        assert_eq!(fs::read(&renamed).unwrap(), b"contents appended");
    }

    #[test]
    fn rejects_paths_with_interior_nul() {
        let root = TestDirectory::new();
        let descriptor = protected_descriptor();
        let path = root.join(OsString::from_wide(&[
            b'n' as u16,
            0,
            b'u' as u16,
            b'l' as u16,
        ]));

        assert_eq!(
            create_directory_with_security(&path, &descriptor)
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidInput
        );
        assert_eq!(
            create_file_with_security(&path, &descriptor)
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidInput
        );
    }
}
