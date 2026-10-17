// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Directory-relative operations on opened handles: opening, creating,
//! hard-linking, and enumerating files relative to an opened directory, and
//! querying the identity of an opened file.

use crate::windows::ObjectAttributes;
use crate::windows::UnicodeString;
use crate::windows::chk_status;
use std::cell::UnsafeCell;
use std::fs;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::AsHandle;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::ptr;
use std::ptr::null_mut;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;
use windows_sys::Wdk::Storage::FileSystem as ntioapi;
use windows_sys::Win32::Foundation::OBJ_CASE_INSENSITIVE;
use windows_sys::Win32::Foundation::STATUS_NO_MORE_FILES;
use windows_sys::Win32::Foundation::STATUS_PENDING;
use windows_sys::Win32::Storage::FileSystem::DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;
use windows_sys::Win32::Storage::FileSystem::FILE_ID_INFO;
use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::FILE_STANDARD_INFO;
use windows_sys::Win32::Storage::FileSystem::FileIdInfo;
use windows_sys::Win32::Storage::FileSystem::FileStandardInfo;
use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

/// Stable identity and EOF for one opened file generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileIdentity {
    pub volume_serial_number: u64,
    pub file_id: [u8; 16],
    pub end_of_file: u64,
}

struct PendingIoStatus(UnsafeCell<IO_STATUS_BLOCK>);

impl PendingIoStatus {
    fn new() -> Self {
        let mut io_status = IO_STATUS_BLOCK::default();
        io_status.Anonymous.Status = STATUS_PENDING;
        Self(UnsafeCell::new(io_status))
    }

    fn as_ptr(&self) -> *mut IO_STATUS_BLOCK {
        self.0.get()
    }

    fn completion_status(&self) -> Option<i32> {
        // SAFETY: IO_STATUS_BLOCK begins with its aligned NTSTATUS. The kernel
        // may update it until it changes from STATUS_PENDING.
        let status = unsafe { &*self.0.get().cast::<AtomicI32>() }.load(Ordering::Acquire);
        (status != STATUS_PENDING).then_some(status)
    }
}

fn ensure_synchronous(file: &fs::File) -> io::Result<()> {
    let mut mode = ntioapi::FILE_MODE_INFORMATION::default();
    let io_status = PendingIoStatus::new();
    let mut status = unsafe {
        ntioapi::NtQueryInformationFile(
            file.as_raw_handle(),
            io_status.as_ptr(),
            (&raw mut mode).cast(),
            size_of::<ntioapi::FILE_MODE_INFORMATION>() as u32,
            ntioapi::FileModeInformation,
        )
    };
    if status == STATUS_PENDING {
        loop {
            if let Some(completion_status) = io_status.completion_status() {
                status = completion_status;
                break;
            }
            // FILE_SKIP_SET_EVENT_ON_HANDLE can suppress file-object signaling.
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    chk_status(status)?;
    if mode.Mode & (ntioapi::FILE_SYNCHRONOUS_IO_ALERT | ntioapi::FILE_SYNCHRONOUS_IO_NONALERT) == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "relative file operation requires a synchronous handle",
        ));
    }
    Ok(())
}

fn validate_relative_component(name: &std::ffi::OsStr, description: &str) -> io::Result<Vec<u16>> {
    let name: Vec<u16> = name.encode_wide().collect();
    // Native relative opens do not apply Win32 dot-component normalization,
    // but dot names still violate this API's single-component contract.
    if name.is_empty()
        || name.contains(&0)
        || name.as_slice() == ['.' as u16]
        || name.as_slice() == ['.' as u16, '.' as u16]
        || name.iter().any(|character| {
            *character == b'/' as u16 || *character == b'\\' as u16 || *character == b':' as u16
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{description} name must be one non-special path component"),
        ));
    }
    Ok(name)
}

/// Opens an existing regular file relative to an opened directory.
///
/// The resulting handle allows read sharing only, so writers and
/// delete/rename attempts are rejected while the handle remains open.
pub fn open_relative_read_only(
    directory: &fs::File,
    name: &std::ffi::OsStr,
) -> io::Result<fs::File> {
    open_relative_file(
        directory,
        name,
        FILE_GENERIC_READ,
        FILE_SHARE_READ,
        ntioapi::FILE_OPEN,
        "open",
    )
}

/// Opens an existing file relative to a directory without denying access to
/// other handles. This is intended for identity checks during publication,
/// not for retaining an immutable restore generation.
pub fn open_relative_for_identity(
    directory: &fs::File,
    name: &std::ffi::OsStr,
) -> io::Result<fs::File> {
    open_relative_file(
        directory,
        name,
        FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        ntioapi::FILE_OPEN,
        "open for identity check",
    )
}

/// Creates a new file relative to an opened directory.
///
/// The returned handle supports writing, identity queries, and hard-link
/// creation.
pub fn create_relative_new(directory: &fs::File, name: &std::ffi::OsStr) -> io::Result<fs::File> {
    open_relative_file(
        directory,
        name,
        FILE_GENERIC_WRITE | FILE_READ_ATTRIBUTES | DELETE,
        FILE_SHARE_READ,
        ntioapi::FILE_CREATE,
        "create",
    )
}

fn open_relative_file(
    directory: &fs::File,
    name: &std::ffi::OsStr,
    desired_access: u32,
    share_access: u32,
    create_disposition: u32,
    operation: &str,
) -> io::Result<fs::File> {
    let name_string = name.to_string_lossy();
    let name = validate_relative_component(name, "file")?;
    let name = UnicodeString::new(&name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file name is too long"))?;
    let mut attributes = ObjectAttributes::new();
    attributes
        .name(&name)
        .root(directory.as_handle())
        .attributes(OBJ_CASE_INSENSITIVE);
    let mut handle = null_mut();
    let mut io_status = IO_STATUS_BLOCK::default();
    let status = unsafe {
        ntioapi::NtCreateFile(
            &mut handle,
            desired_access | SYNCHRONIZE,
            attributes.as_ptr(),
            &mut io_status,
            ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            share_access,
            create_disposition,
            ntioapi::FILE_NON_DIRECTORY_FILE
                | ntioapi::FILE_OPEN_REPARSE_POINT
                | ntioapi::FILE_SYNCHRONOUS_IO_NONALERT,
            ptr::null(),
            0,
        )
    };
    chk_status(status).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to {operation} {name_string}: {error}"),
        )
    })?;
    Ok(unsafe { fs::File::from_raw_handle(handle) })
}

/// Creates a hard link to `source` relative to an exact opened directory.
///
/// `source` must have been opened for synchronous I/O and with delete access.
pub fn hard_link_relative(
    source: &fs::File,
    directory: &fs::File,
    name: &std::ffi::OsStr,
) -> io::Result<()> {
    ensure_synchronous(source)?;
    let name = validate_relative_component(name, "hard-link")?;

    let name_bytes = name
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|length| u32::try_from(length).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "hard-link name is too long"))?;
    let header_bytes = std::mem::offset_of!(ntioapi::FILE_LINK_INFORMATION, FileName);
    let information_bytes = header_bytes
        .checked_add(name_bytes as usize)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "hard-link name is too long"))?
        .max(size_of::<ntioapi::FILE_LINK_INFORMATION>());
    let information_length = u32::try_from(information_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "hard-link name is too long"))?;
    let mut information = vec![0_u64; information_bytes.div_ceil(size_of::<u64>())];
    let information = information
        .as_mut_ptr()
        .cast::<ntioapi::FILE_LINK_INFORMATION>();
    let mut io_status = IO_STATUS_BLOCK::default();

    // SAFETY: `information` is aligned and sized for FILE_LINK_INFORMATION
    // followed by the exact UTF-16 name. Both handles remain live for the
    // duration of the synchronous call, which does not retain the buffer.
    unsafe {
        (*information).Anonymous.ReplaceIfExists = false;
        (*information).RootDirectory = directory.as_raw_handle();
        (*information).FileNameLength = name_bytes;
        ptr::copy_nonoverlapping(
            name.as_ptr(),
            (&raw mut (*information).FileName).cast::<u16>(),
            name.len(),
        );
        chk_status(ntioapi::NtSetInformationFile(
            source.as_raw_handle(),
            &mut io_status,
            information.cast(),
            information_length,
            ntioapi::FileLinkInformation,
        ))?;
    }
    Ok(())
}

/// Enumerates names relative to an opened directory handle.
///
/// The directory must have been opened for synchronous I/O.
pub fn directory_entry_names(directory: &fs::File) -> io::Result<Vec<std::ffi::OsString>> {
    const BUFFER_SIZE: usize = 4096;
    const BUFFER_WORDS: usize = BUFFER_SIZE / size_of::<u64>();

    ensure_synchronous(directory)?;
    let mut names = Vec::new();
    let mut restart_scan = true;
    loop {
        let mut buffer = [0_u64; BUFFER_WORDS];
        let mut io_status = IO_STATUS_BLOCK::default();
        let status = unsafe {
            ntioapi::NtQueryDirectoryFile(
                directory.as_raw_handle(),
                null_mut(),
                None,
                ptr::null(),
                &mut io_status,
                buffer.as_mut_ptr().cast(),
                BUFFER_SIZE as u32,
                ntioapi::FileNamesInformation,
                true,
                ptr::null(),
                restart_scan,
            )
        };
        restart_scan = false;
        if status == STATUS_NO_MORE_FILES {
            break;
        }
        chk_status(status)?;

        let returned = io_status.Information;
        let header_size = std::mem::offset_of!(ntioapi::FILE_NAMES_INFORMATION, FileName);
        if returned < header_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "directory query returned a truncated entry",
            ));
        }
        let entry = unsafe { &*buffer.as_ptr().cast::<ntioapi::FILE_NAMES_INFORMATION>() };
        let name_length = usize::try_from(entry.FileNameLength).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "directory entry name length exceeds usize",
            )
        })?;
        if !name_length.is_multiple_of(2) || header_size + name_length > returned {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "directory query returned an invalid entry name",
            ));
        }
        let name = unsafe {
            std::slice::from_raw_parts(entry.FileName.as_ptr(), name_length / size_of::<u16>())
        };
        let name = std::ffi::OsString::from_wide(name);
        if name != "." && name != ".." {
            names.push(name);
        }
    }
    Ok(names)
}

/// Queries `FILE_ID_INFO` and the current EOF for an opened file.
pub fn file_identity(file: &fs::File) -> io::Result<FileIdentity> {
    let mut identity = FILE_ID_INFO::default();
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&raw mut identity).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut standard = FILE_STANDARD_INFO::default();
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileStandardInfo,
            (&raw mut standard).cast(),
            size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    if standard.Directory || standard.DeletePending {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handle is not an available regular file",
        ));
    }
    let end_of_file = u64::try_from(standard.EndOfFile)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file has a negative EOF"))?;
    Ok(FileIdentity {
        volume_serial_number: identity.VolumeSerialNumber,
        file_id: identity.FileId.Identifier,
        end_of_file,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Read;
    use std::io::Write;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;
    use std::path::PathBuf;
    use test_with_tracing::test;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            loop {
                let mut id = [0; 16];
                getrandom::fill(&mut id).unwrap();
                let path = std::env::temp_dir()
                    .join(format!("pal-relative-{:032x}", u128::from_ne_bytes(id)));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => panic!("failed to create test directory: {error}"),
                }
            }
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn open_directory(path: &Path, overlapped: bool) -> fs::File {
        let flags = FILE_FLAG_BACKUP_SEMANTICS | if overlapped { FILE_FLAG_OVERLAPPED } else { 0 };
        OpenOptions::new()
            .read(true)
            .custom_flags(flags)
            .open(path)
            .unwrap()
    }

    #[test]
    fn relative_name_must_be_one_non_special_component() {
        for name in [
            "",
            ".",
            "..",
            "dir\\file",
            "dir/file",
            "file:stream",
            "nul\0name",
        ] {
            let error = validate_relative_component(name.as_ref(), "file").unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{name:?}");
        }

        for name in ["file", ".file", "..."] {
            assert!(
                validate_relative_component(name.as_ref(), "file").is_ok(),
                "{name:?}"
            );
        }
    }

    #[test]
    fn relative_file_lifecycle_uses_the_opened_directory() {
        let root = TestDirectory::new();
        let directory = open_directory(root.path(), false);
        let mut source = create_relative_new(&directory, "source".as_ref()).unwrap();
        source.write_all(b"contents").unwrap();
        let source_identity = file_identity(&source).unwrap();

        hard_link_relative(&source, &directory, "link".as_ref()).unwrap();
        drop(source);
        let mut link = open_relative_read_only(&directory, "link".as_ref()).unwrap();
        let mut contents = Vec::new();
        link.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"contents");
        assert_eq!(file_identity(&link).unwrap(), source_identity);
        assert_eq!(
            file_identity(&open_relative_for_identity(&directory, "source".as_ref()).unwrap())
                .unwrap(),
            source_identity
        );

        let mut names: Vec<_> = directory_entry_names(&directory)
            .unwrap()
            .into_iter()
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["link".to_owned(), "source".to_owned()]);
        assert_eq!(
            create_relative_new(&directory, "source".as_ref())
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn pending_native_operations_are_rejected() {
        let root = TestDirectory::new();
        let directory = open_directory(root.path(), false);
        let asynchronous_directory = open_directory(root.path(), true);
        let asynchronous_source = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .custom_flags(FILE_FLAG_OVERLAPPED)
            .open(root.path().join("asynchronous"))
            .unwrap();

        assert_eq!(
            hard_link_relative(&asynchronous_source, &directory, "link".as_ref())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            directory_entry_names(&asynchronous_directory)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
