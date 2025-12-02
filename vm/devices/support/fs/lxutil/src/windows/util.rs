// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::fs;
use super::macros::file_information_classes;
use crate::SetAttributes;
use crate::SetTime;
use crate::windows::path;
use ::windows::Wdk;
use ::windows::Wdk::Storage::FileSystem;
use ::windows::Wdk::System::SystemServices;
use ::windows::Win32::Foundation;
use ::windows::Win32::Security;
use ::windows::Win32::Security as W32Sec;
use ::windows::Win32::Storage::FileSystem as W32Fs;
use ::windows::Win32::System::Ioctl;
use ::windows::Win32::System::SystemServices as W32Ss;
use ::windows::Win32::System::Threading;
use ::windows::core::PCWSTR;
use headervec::HeaderVec;
use pal::windows;
use std::ffi;
use std::mem;
use std::mem::offset_of;
use std::os::windows::prelude::*;
use std::path::Path;
use std::ptr;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

// Largest security descriptor seen across all threads.
//
// N.B. This is the same base value used for ObGetObjectSecurity.
static LX_UTIL_FS_SECURITY_DESCRIPTOR_SIZE: AtomicUsize = AtomicUsize::new(256);

// Minimum permissions needed to access a file's metadata.
pub const MINIMUM_PERMISSIONS: W32Fs::FILE_ACCESS_RIGHTS = W32Fs::FILE_ACCESS_RIGHTS(
    W32Fs::FILE_READ_ATTRIBUTES.0 | W32Fs::FILE_READ_EA.0 | W32Fs::READ_CONTROL.0,
);

// The following constant is a bias that offsets the NT time to the
// POSIX time origin. From C:
// static CONST LARGE_INTEGER LxPosixEpochOffset = {0xd53e8000, 0x019db1de};
const LX_POSIX_EPOCH_OFFSET: i64 = 0xd53e8000 + (0x019db1de << 32);

const LX_UTIL_NT_UNIT_PER_SEC: i64 = 10000000;
const LX_UTIL_NANO_SEC_PER_NT_UNIT: i64 = 100;

const LX_UTIL_NT_SYMLINK_ESCAPE_TARGET: u32 = 0x1;

/// A trait that maps a file information struct for calling into `NtSetInformationFile`, `NtQueryInformationFile` and
/// other methods that accept a `FileInformationClass`.
pub trait FileInformationClass: Default {
    /// The [`FileSystem::FILE_INFORMATION_CLASS`] passed to `FileInformationClass` in different NT methods.
    fn file_information_class(&self) -> FileSystem::FILE_INFORMATION_CLASS;

    /// A ptr and len describing this struct to be used for `FileInformation` in different NT methods.
    ///
    /// # Safety
    /// The intention for these methods are to be used directly with the C API. The ptr, len combo returned may access
    /// padding bytes which would be UB in Rust.
    fn as_ptr_len(&self) -> (*const u8, usize);

    /// A ptr and len describing this struct to be used for `FileInformation` in different NT methods.
    ///
    /// # Safety
    /// The intention for these methods are to be used directly with the C API. The ptr, len combo returned may access
    /// padding bytes which would be UB in Rust.
    fn as_ptr_len_mut(&mut self) -> (*mut u8, usize);
}

file_information_classes!(
    FileSystem::FILE_BASIC_INFORMATION = FileSystem::FileBasicInformation;
    FileSystem::FILE_STANDARD_INFORMATION = FileSystem::FileStandardInformation;
    SystemServices::FILE_ATTRIBUTE_TAG_INFORMATION = FileSystem::FileAttributeTagInformation;
    FileSystem::FILE_DISPOSITION_INFORMATION = FileSystem::FileDispositionInformation;
    FileSystem::FILE_DISPOSITION_INFORMATION_EX = FileSystem::FileDispositionInformationEx;
    FileSystem::FILE_STAT_INFORMATION = FileSystem::FileStatInformation;
    FileSystem::FILE_STAT_LX_INFORMATION = FileSystem::FileStatLxInformation;
    FileSystem::FILE_ALL_INFORMATION = FileSystem::FileAllInformation;
    FileSystem::FILE_CASE_SENSITIVE_INFORMATION = FileSystem::FileCaseSensitiveInformation;
    SystemServices::FILE_END_OF_FILE_INFORMATION = FileSystem::FileEndOfFileInformation;
);

// Open a file using NtCreateFile.
// Returns the create result from the IO_STATUS_BLOCK along with the handle.
// N.B. The handle will always be opened synchronously.
pub fn open_relative_file(
    root: Option<&OwnedHandle>,
    path: &Path,
    desired_access: W32Fs::FILE_ACCESS_RIGHTS,
    creation_disposition: FileSystem::NTCREATEFILE_CREATE_DISPOSITION,
    file_attributes: W32Fs::FILE_FLAGS_AND_ATTRIBUTES,
    create_options: FileSystem::NTCREATEFILE_CREATE_OPTIONS,
    ea_buffer: Option<&[u8]>,
) -> lx::Result<(OwnedHandle, usize)> {
    let path = dos_to_nt_path(root, path)?;
    let mut oa = windows::ObjectAttributes::new();
    oa.name(&path)
        .attributes(Foundation::OBJ_CASE_INSENSITIVE.0);
    if let Some(handle) = root {
        oa.root(handle.as_handle());
    }

    let mut handle = Default::default();
    let mut iosb = Default::default();
    let (ea_ptr, ea_len) = if let Some(ea) = ea_buffer {
        (Some(ea.as_ptr().cast()), ea.len() as u32)
    } else {
        (None, 0)
    };
    unsafe {
        let status = FileSystem::NtCreateFile(
            &mut handle,
            desired_access | W32Fs::SYNCHRONIZE,
            oa.as_ptr().cast(),
            &mut iosb,
            None,
            file_attributes,
            W32Fs::FILE_SHARE_READ | W32Fs::FILE_SHARE_WRITE | W32Fs::FILE_SHARE_DELETE,
            creation_disposition,
            create_options | FileSystem::FILE_SYNCHRONOUS_IO_NONALERT,
            ea_ptr,
            ea_len,
        );

        let _ = check_status(status)?;
        Ok((OwnedHandle::from_raw_handle(handle.0), iosb.Information))
    }
}

// Reopen an existing file handle with different permissions.
pub fn reopen_file(
    file: &OwnedHandle,
    desired_access: W32Fs::FILE_ACCESS_RIGHTS,
) -> lx::Result<OwnedHandle> {
    let (handle, _) = open_relative_file(
        Some(file),
        "".as_ref(),
        desired_access,
        FileSystem::FILE_OPEN,
        Default::default(),
        FileSystem::FILE_OPEN_REPARSE_POINT,
        None,
    )?;

    Ok(handle)
}

// Get a token that can be used for a user-mode access check.
fn get_token_for_access_check() -> lx::Result<OwnedHandle> {
    let mut client_token_raw = Foundation::HANDLE::default();
    let mut duplicate_token_raw = Foundation::HANDLE::default();
    // SAFETY: Calling Win32 API as documented.
    let result = unsafe {
        check_status(FileSystem::NtOpenThreadToken(
            Threading::GetCurrentThread(),
            Security::TOKEN_QUERY.0 | Security::TOKEN_DUPLICATE.0,
            true,
            &mut client_token_raw,
        ))
    };

    if let Err(e) = result {
        // Use the process token if there's no token.
        if e.value() == Foundation::STATUS_NO_TOKEN.0 {
            // SAFETY: Calling Win32 API as documented.
            unsafe {
                let _ = check_status(FileSystem::NtOpenProcessToken(
                    Threading::GetCurrentProcess(),
                    Security::TOKEN_QUERY.0 | Security::TOKEN_DUPLICATE.0,
                    &mut client_token_raw,
                ))?;
            }
        } else {
            return Err(e);
        }
    }

    // Create the RAII handle now so it gets closed on drop if we need
    // to create a duplicate token.
    //
    // SAFETY: The validity of the handle has been checked by verifying
    // the success of the previous Win32 calls.
    let client_token = unsafe { OwnedHandle::from_raw_handle(client_token_raw.0) };

    let mut token_statistics = Security::TOKEN_STATISTICS::default();
    let mut bytes_written: u32 = 0;
    // Make sure that it's an impersonation token (NtAccessCheck requires one).
    // SAFETY: Calling Win32 API as documented.
    unsafe {
        let _ = check_status(FileSystem::NtQueryInformationToken(
            client_token_raw,
            Security::TokenStatistics,
            Some((ptr::from_mut::<Security::TOKEN_STATISTICS>(&mut token_statistics)).cast()),
            size_of::<Security::TOKEN_STATISTICS>() as u32,
            &mut bytes_written,
        ))?;
    }

    // If it's not an impersonation token, create one.
    if token_statistics.TokenType != W32Sec::TokenImpersonation {
        let security_qos = W32Sec::SECURITY_QUALITY_OF_SERVICE {
            Length: size_of::<W32Sec::SECURITY_QUALITY_OF_SERVICE>() as _,
            ImpersonationLevel: W32Sec::SecurityImpersonation,
            ContextTrackingMode: if W32Sec::SECURITY_DYNAMIC_TRACKING {
                1
            } else {
                0
            },
            EffectiveOnly: false,
        };
        let mut object_attributes = Wdk::Foundation::OBJECT_ATTRIBUTES::default();
        object_attributes.SecurityQualityOfService =
            ptr::from_ref::<W32Sec::SECURITY_QUALITY_OF_SERVICE>(&security_qos).cast();

        // SAFETY: Calling Win32 API as documented.
        unsafe {
            let _ = check_status(FileSystem::NtDuplicateToken(
                client_token_raw,
                W32Sec::TOKEN_QUERY.0,
                Some(&object_attributes),
                false,
                W32Sec::TokenImpersonation,
                &mut duplicate_token_raw,
            ))?;
            return Ok(OwnedHandle::from_raw_handle(duplicate_token_raw.0));
        }
    }

    Ok(client_token)
}

// Check if the user has the requested permissions on a file and
// return the granted access.
pub fn check_security(
    file: &OwnedHandle,
    desired_access: W32Fs::FILE_ACCESS_RIGHTS,
) -> lx::Result<u32> {
    let initial_size = LX_UTIL_FS_SECURITY_DESCRIPTOR_SIZE.load(Ordering::Relaxed);

    // NtQuerySecurityObject returns a SECURITY_DESCRIPTOR in self-relative form, so allocate a buffer
    // with the SECURITY_DESCRIPTOR at the head and a byte buffer at the end. We don't actually use
    // the rest of the bytes, but we need to allocate the buffer to pass to NtQuerySecurityObject.
    let mut sd = HeaderVec::<Security::SECURITY_DESCRIPTOR, u8, 1>::with_capacity(
        Security::SECURITY_DESCRIPTOR::default(),
        initial_size - size_of::<Security::SECURITY_DESCRIPTOR>(),
    );
    let mut length_needed: u32 = 0;

    // Call NtQuerySecurityObject until we pass a big enough buffer.
    while let Err(e) =
        // unsafe: calling Win32 API as documented.
        unsafe {
            // If the buffer was too small, try again and remember the size
            // for future calls. This is the same strategy used by
            // ObGetObjectSecurity.
            check_status(FileSystem::NtQuerySecurityObject(
                Foundation::HANDLE(file.as_raw_handle()),
                (Security::OWNER_SECURITY_INFORMATION
                    | Security::GROUP_SECURITY_INFORMATION
                    | Security::DACL_SECURITY_INFORMATION)
                    .0,
                Some(Security::PSECURITY_DESCRIPTOR(sd.as_mut_ptr().cast())),
                initial_size as u32,
                &mut length_needed,
            ))
        }
    {
        if e.value() == Foundation::STATUS_BUFFER_TOO_SMALL.0 {
            LX_UTIL_FS_SECURITY_DESCRIPTOR_SIZE.store(length_needed as usize, Ordering::Relaxed);
            sd.reserve_tail(length_needed as usize);
        } else {
            return Err(e);
        }
    }

    // SAFETY: The tail elements are guaranteed to be initialized.
    unsafe {
        sd.set_tail_len(length_needed as usize - size_of::<Security::SECURITY_DESCRIPTOR>());
    }
    let client_token = get_token_for_access_check()?;
    let generic_mapping = W32Sec::GENERIC_MAPPING {
        GenericRead: W32Fs::FILE_GENERIC_READ.0,
        GenericWrite: W32Fs::FILE_GENERIC_WRITE.0,
        GenericExecute: W32Fs::FILE_GENERIC_EXECUTE.0,
        GenericAll: W32Fs::FILE_ALL_ACCESS.0,
    };
    let mut privilege_set = W32Sec::PRIVILEGE_SET::default();
    let mut privilege_set_length = size_of::<W32Sec::PRIVILEGE_SET>() as u32;
    let mut granted_access = 0;
    let mut access_status = ::windows::core::BOOL::default();

    // SAFETY: calling Win32 API as documented.
    unsafe {
        W32Sec::AccessCheck(
            Security::PSECURITY_DESCRIPTOR(sd.as_mut_ptr().cast()),
            Foundation::HANDLE(client_token.as_raw_handle()),
            desired_access.0,
            &generic_mapping,
            Some(ptr::from_mut::<W32Sec::PRIVILEGE_SET>(&mut privilege_set)),
            &mut privilege_set_length,
            &mut granted_access,
            &mut access_status,
        )
        .map_err(|_| lx::Error::EACCES)?
    };

    if access_status == Foundation::FALSE {
        Err(lx::Error::EACCES)
    } else {
        Ok(granted_access)
    }
}

pub struct LxStatInformation {
    pub stat: FileSystem::FILE_STAT_LX_INFORMATION,
    pub symlink_len: Option<u32>,
    pub is_app_execution_alias: bool,
}

// Get file attributes from a file handle.
pub fn get_attributes_by_handle(
    fs_context: &fs::FsContext,
    state: &super::VolumeState,
    handle: &OwnedHandle,
) -> lx::Result<LxStatInformation> {
    let stat = fs::query_stat_lx_information(handle, fs_context)?;

    // For NT symlinks and V2 LX symlinks, the size of the file is not correct, and must be
    // determined based on the reparse data.
    let symlink_len = if is_symlink(stat.FileAttributes, stat.ReparseTag) && stat.EndOfFile == 0 {
        // Return 0 here on failure to duplicate LxUtil behavior
        Some(fs::read_link_length(handle, state).unwrap_or(0))
    } else {
        None
    };
    let is_app_execution_alias =
        stat.EndOfFile == 0 && fs::is_app_exec_link(stat.FileAttributes, stat.ReparseTag);

    Ok(LxStatInformation {
        stat,
        symlink_len,
        is_app_execution_alias,
    })
}

// Get file attributes from a file name.
// This may open the file if NtQueryInformationByName is not supported.
pub fn get_attributes(
    fs_context: &fs::FsContext,
    state: &super::VolumeState,
    root_handle: Option<&OwnedHandle>,
    path: &Path,
    existing_handle: Option<&OwnedHandle>,
) -> lx::Result<LxStatInformation> {
    if let Some(existing_handle) = existing_handle {
        return get_attributes_by_handle(fs_context, state, existing_handle);
    }

    // If NtQueryInformationByName is supported, use it.
    if fs_context.compatibility_flags.supports_query_by_name() {
        let pathu = dos_to_nt_path(root_handle, path)?;

        let stat = fs::query_stat_lx_information_by_name(fs_context, root_handle, &pathu)?;

        // For NT symlinks and V2 LX symlinks, the size of the file is not correct, and must be
        // determined based on the reparse data, which requires opening the file.
        let symlink_len = if is_symlink(stat.FileAttributes, stat.ReparseTag) && stat.EndOfFile == 0
        {
            let symlink_len = if let Ok((handle, _)) = open_relative_file(
                root_handle,
                path,
                MINIMUM_PERMISSIONS,
                FileSystem::FILE_OPEN,
                Default::default(),
                FileSystem::FILE_OPEN_REPARSE_POINT,
                None,
            ) {
                // Return 0 here on failure to duplicate LxUtil behavior
                fs::read_link_length(&handle, state).unwrap_or(0)
            } else {
                0
            };
            Some(symlink_len)
        } else {
            None
        };
        let is_app_execution_alias =
            stat.EndOfFile.eq(&0) && fs::is_app_exec_link(stat.FileAttributes, stat.ReparseTag);

        Ok(LxStatInformation {
            stat,
            symlink_len,
            is_app_execution_alias,
        })
    } else {
        // If NtQueryInformationByName is not supported, open the file to query attributes.
        let (handle, _) = open_relative_file(
            root_handle,
            path,
            MINIMUM_PERMISSIONS,
            FileSystem::FILE_OPEN,
            Default::default(),
            FileSystem::FILE_OPEN_REPARSE_POINT,
            None,
        )?;

        get_attributes_by_handle(fs_context, state, &handle)
    }
}

// Determine if a name is a reserved legacy DOS device name
pub fn is_dos_device_name(name: &ffi::OsStr) -> lx::Result<bool> {
    let nameu = widestring::U16CString::from_os_str(name).map_err(|_| lx::Error::EINVAL)?;
    if unsafe { FileSystem::RtlIsDosDeviceName_U(PCWSTR(nameu.as_slice_with_nul().as_ptr())) } == 0
    {
        Ok(false)
    } else {
        Ok(true)
    }
}

// Determine if a file is symlink based on its reparse tag.
pub fn is_symlink(attributes: u32, reparse_tag: u32) -> bool {
    attributes & W32Fs::FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        && (reparse_tag == W32Ss::IO_REPARSE_TAG_SYMLINK
            || reparse_tag == W32Ss::IO_REPARSE_TAG_MOUNT_POINT
            || reparse_tag == FileSystem::IO_REPARSE_TAG_LX_SYMLINK as u32)
}

// Convert an NTSTATUS into an lx::Result.
// Returns the status value on success in case it held a non-error value.
pub fn check_status(status: Foundation::NTSTATUS) -> lx::Result<Foundation::NTSTATUS> {
    if status.0 < 0 {
        Err(nt_status_to_lx(status))
    } else {
        Ok(status)
    }
}

// Same as check_status, but used for read/write operations to handle EBADF and EOF.
pub fn check_status_rw(status: Foundation::NTSTATUS) -> lx::Result<Foundation::NTSTATUS> {
    if status.0 < 0 {
        match status {
            Foundation::STATUS_ACCESS_DENIED => Err(lx::Error::EBADF),
            Foundation::STATUS_END_OF_FILE => Ok(Foundation::STATUS_SUCCESS),
            _ => Err(nt_status_to_lx(status)),
        }
    } else {
        Ok(status)
    }
}

// Convert a DOS path to an NT path depending on whether the root is set.
#[expect(clippy::join_absolute_paths)] // https://github.com/rust-lang/rust-clippy/issues/12244
pub fn dos_to_nt_path(
    root: Option<&OwnedHandle>,
    path: &Path,
) -> lx::Result<windows::UnicodeString> {
    let path = if root.is_none() {
        // Windows has legacy behavior where specifying just a drive letter will return the last path on that drive
        // from the current cmd.exe instance. This is likely not the intended behavior and is generally not safe.
        if path.as_os_str().len() == 2 && path.to_str().is_some_and(|s| s.ends_with(':')) {
            windows::dos_to_nt_path(path.join("\\"))?
        } else {
            windows::dos_to_nt_path(path)?
        }
    } else {
        path.try_into().map_err(|_| lx::Error::ENAMETOOLONG)?
    };

    Ok(path)
}

// Convert Linux open flags o a Windows access mask.
pub fn open_flags_to_access_mask(flags: i32) -> W32Fs::FILE_ACCESS_RIGHTS {
    let mut mask = MINIMUM_PERMISSIONS;
    mask |= match flags & lx::O_ACCESS_MASK {
        lx::O_RDONLY => W32Fs::FILE_GENERIC_READ,
        lx::O_WRONLY => W32Fs::FILE_GENERIC_WRITE,
        lx::O_RDWR => W32Fs::FILE_GENERIC_READ | W32Fs::FILE_GENERIC_WRITE,
        _ => Default::default(),
    };

    if flags & lx::O_APPEND != 0 {
        // Remove the write permission; the generic permission already includes FILE_APPEND_DATA.
        mask &= !W32Fs::FILE_WRITE_DATA;
    }

    if flags & lx::O_TRUNC != 0 {
        // Truncate on Linux is allowed with O_RDONLY, but requires write access on Windows.
        mask |= W32Fs::FILE_WRITE_DATA;
    }

    mask
}

// Convert Linux open flags to file attributes and create options.
pub fn open_flags_to_attributes_options(
    flags: i32,
) -> (
    W32Fs::FILE_FLAGS_AND_ATTRIBUTES,
    FileSystem::NTCREATEFILE_CREATE_OPTIONS,
) {
    let mut file_attributes = Default::default();
    let mut create_options = Default::default();

    if flags & lx::O_DIRECTORY != 0 {
        file_attributes |= W32Fs::FILE_ATTRIBUTE_DIRECTORY;
        create_options |= FileSystem::FILE_DIRECTORY_FILE;
    } else {
        file_attributes |= W32Fs::FILE_ATTRIBUTE_NORMAL;
    }

    if flags & lx::O_NOFOLLOW != 0 {
        create_options |= FileSystem::FILE_OPEN_REPARSE_POINT;
    }

    (file_attributes, create_options)
}

// Convert Linux open flags to a create disposition.
pub fn open_flags_to_disposition(flags: i32) -> FileSystem::NTCREATEFILE_CREATE_DISPOSITION {
    if flags & lx::O_CREAT != 0 {
        if flags & lx::O_EXCL != 0 {
            FileSystem::FILE_CREATE
        } else {
            FileSystem::FILE_OPEN_IF
        }
    } else {
        FileSystem::FILE_OPEN
    }
}

fn override_mode(actual: u32, hard_coded: u32) -> u32 {
    actual & lx::S_IFMT
        | hard_coded
        | if (actual & lx::S_IFDIR) != 0 {
            // Add 'x' bit to 'r' for directories, to allow performing directory listing
            ((hard_coded & 0o444) >> 2) | lx::S_IFDIR
        } else {
            0
        }
}

// Converts Windows file information to stat attributes.
pub fn file_info_to_stat(
    fs_context: &fs::FsContext,
    information: &mut LxStatInformation,
    options: &crate::LxVolumeOptions,
    block_size: u32,
) -> lx::Result<lx::Stat> {
    let mut stat = fs::get_lx_attr(
        fs_context,
        &mut information.stat,
        fs::LX_UTIL_FS_CALLER_HAS_TRAVERSE_PRIVILEGE,
        block_size,
        options.default_uid,
        options.default_gid,
        options.umask,
        options.dmask,
        options.fmask,
    )?;

    // The uid, gid and mode options are applied to all files, even if they have metadata.
    if let Some(uid) = options.uid {
        stat.uid = uid;
    }
    if let Some(gid) = options.gid {
        stat.gid = gid;
    }
    if let Some(mode) = options.mode {
        stat.mode = override_mode(stat.mode, mode);
    }
    if let Some(symlink_len) = information.symlink_len {
        stat.file_size = symlink_len as u64;
    } else if information.is_app_execution_alias {
        stat.file_size = fs::LX_UTIL_PE_HEADER_SIZE as u64;
    }
    Ok(stat)
}

// Create the reparse buffer for an NT symlink.
pub fn create_nt_link_reparse_buffer(target: &ffi::OsStr, flags: u32) -> lx::Result<Vec<u8>> {
    let mut link_target: windows::UnicodeString =
        target.try_into().map_err(|_| lx::Error::EINVAL)?;

    // The buffer contains the target name twice, once for the actual target name
    // and once for the display name.
    let local_size = offset_of!(FileSystem::REPARSE_DATA_BUFFER, Anonymous)
        + offset_of!(FileSystem::REPARSE_DATA_BUFFER_0_0, PathBuffer)
        + (link_target.length() * 2);

    if local_size > u16::MAX as usize {
        return Err(lx::Error::ENAMETOOLONG);
    }

    let mut reparse = vec![0; local_size];

    // SAFETY: The buffer is large enough to hold the reparse data.
    // This is necessary because unfortunately the Windows types don't
    // implement the necessary traits to use zerocopy.
    unsafe {
        *(reparse.as_mut_ptr().cast()) = FileSystem::REPARSE_DATA_BUFFER {
            ReparseTag: W32Ss::IO_REPARSE_TAG_SYMLINK,
            ReparseDataLength: ((local_size
                - offset_of!(FileSystem::REPARSE_DATA_BUFFER, Anonymous))
                as u16),
            Reserved: 0,
            Anonymous: FileSystem::REPARSE_DATA_BUFFER_0 {
                SymbolicLinkReparseBuffer: FileSystem::REPARSE_DATA_BUFFER_0_0 {
                    SubstituteNameOffset: link_target.length() as u16,
                    SubstituteNameLength: link_target.length() as u16,
                    PrintNameOffset: 0,
                    PrintNameLength: link_target.length() as u16,
                    Flags: FileSystem::SYMLINK_FLAG_RELATIVE,
                    PathBuffer: [0; 1], // Actual data follows
                },
            },
        };
    }

    if flags & LX_UTIL_NT_SYMLINK_ESCAPE_TARGET != 0 {
        path::unescape_path_in_place(link_target.as_mut_slice());
    }

    let path_buffer = &mut reparse[offset_of!(FileSystem::REPARSE_DATA_BUFFER, Anonymous)
        + offset_of!(FileSystem::REPARSE_DATA_BUFFER_0_0, PathBuffer)..];

    let link_bytes = link_target.as_slice().as_bytes();
    path_buffer[..link_bytes.len()].copy_from_slice(link_bytes);
    path_buffer[link_bytes.len()..(link_bytes.len() * 2)].copy_from_slice(link_bytes);

    Ok(reparse)
}

// Applies a reparse point to a file.
pub fn set_reparse_point(handle: &OwnedHandle, reparse_buffer: &[u8]) -> lx::Result<()> {
    let mut iosb = Default::default();
    unsafe {
        let _ = check_status(FileSystem::NtFsControlFile(
            Foundation::HANDLE(handle.as_raw_handle()),
            None,
            None,
            None,
            &mut iosb,
            Ioctl::FSCTL_SET_REPARSE_POINT,
            Some(reparse_buffer.as_ptr().cast()),
            reparse_buffer
                .len()
                .try_into()
                .map_err(|_| lx::Error::EINVAL)?,
            None,
            0,
        ))?;
    }

    Ok(())
}

// Gets file information from a handle.
pub fn query_information_file<T: FileInformationClass>(handle: &OwnedHandle) -> lx::Result<T> {
    let mut iosb = Default::default();
    let mut info: T = Default::default();
    let (buf, len) = info.as_ptr_len_mut();

    // SAFETY: Calling NtQueryInformationFile as documented.
    unsafe {
        let _ = check_status(FileSystem::NtQueryInformationFile(
            Foundation::HANDLE(handle.as_raw_handle()),
            &mut iosb,
            buf.cast(),
            len.try_into().unwrap(),
            info.file_information_class(),
        ))?;

        Ok(info)
    }
}

// Gets file information from a handle.
pub fn query_information_file_by_name<T: FileInformationClass>(
    parent_handle: Option<&OwnedHandle>,
    path: &windows::UnicodeString,
) -> lx::Result<T> {
    let mut iosb = Default::default();
    let mut info: T = Default::default();
    let root_handle = if let Some(ptr) = parent_handle {
        Foundation::HANDLE(ptr.as_raw_handle())
    } else {
        Foundation::HANDLE(ptr::null_mut())
    };

    // windows-rs does not include the InitializeObjectAttributes macro; this is the
    // recommended method of initialization. https://github.com/microsoft/windows-rs/issues/3183
    let obj_attr = Wdk::Foundation::OBJECT_ATTRIBUTES {
        Length: size_of::<Wdk::Foundation::OBJECT_ATTRIBUTES>() as _,
        RootDirectory: root_handle,
        ObjectName: path.as_ptr().cast(),
        Attributes: Foundation::OBJ_FORCE_ACCESS_CHECK as _,
        SecurityDescriptor: ptr::null_mut(),
        SecurityQualityOfService: ptr::null_mut(),
    };
    let (buf, len) = info.as_ptr_len_mut();

    // SAFETY: Calling NtQueryInformationFile as documented.
    unsafe {
        let _ = check_status(FileSystem::NtQueryInformationByName(
            &obj_attr,
            &mut iosb,
            buf.cast(),
            len.try_into().unwrap(),
            info.file_information_class(),
        ))?;

        Ok(info)
    }
}

/// Sets file information on a handle.
pub fn set_information_file<T: FileInformationClass>(
    handle: &OwnedHandle,
    info: &T,
) -> lx::Result<()> {
    let mut iosb = Default::default();
    let (buf, len) = info.as_ptr_len();

    // SAFETY: Calling NtSetInformationFile as documented.
    unsafe {
        let _ = check_status(FileSystem::NtSetInformationFile(
            Foundation::HANDLE(handle.as_raw_handle()),
            &mut iosb,
            buf.cast(),
            len.try_into().unwrap(),
            info.file_information_class(),
        ))?;

        Ok(())
    }
}

// Sets the read-only attribute on a file
pub fn set_readonly_attribute(
    handle: &OwnedHandle,
    attributes: u32,
    readonly: bool,
) -> lx::Result<()> {
    let info = FileSystem::FILE_BASIC_INFORMATION {
        FileAttributes: if readonly {
            attributes | W32Fs::FILE_ATTRIBUTE_READONLY.0
        } else {
            // FILE_ATTRIBUTE_NORMAL is ignored when other attributes are set,
            // so adding it here is safe and ensures the value won't be zero.
            (attributes & !(W32Fs::FILE_ATTRIBUTE_READONLY.0)) | W32Fs::FILE_ATTRIBUTE_NORMAL.0
        },
        ..Default::default()
    };

    set_information_file(handle, &info)
}

// Convert an NTSTATUS to a Linux error code.
pub fn nt_status_to_lx(status: Foundation::NTSTATUS) -> lx::Error {
    lx::Error::from_lx(match status {
        Foundation::STATUS_SUCCESS => 0,
        Foundation::STATUS_OBJECTID_NOT_FOUND => lx::ESRCH,
        Foundation::STATUS_DIRECTORY_NOT_EMPTY => lx::ENOTEMPTY,
        Foundation::STATUS_OBJECT_NAME_EXISTS
        | Foundation::STATUS_OBJECT_NAME_COLLISION
        | Foundation::STATUS_OBJECTID_EXISTS
        | Foundation::STATUS_DUPLICATE_OBJECTID => lx::EEXIST,
        Foundation::STATUS_ADDRESS_ALREADY_EXISTS => lx::EADDRINUSE,
        Foundation::STATUS_ACCESS_VIOLATION => lx::EFAULT,
        Foundation::STATUS_INSUFFICIENT_RESOURCES
        | Foundation::STATUS_NO_MEMORY
        | Foundation::STATUS_COMMITMENT_LIMIT
        | Foundation::STATUS_GRAPHICS_NO_VIDEO_MEMORY
        | Foundation::STATUS_PAGEFILE_QUOTA => lx::ENOMEM,
        Foundation::STATUS_IN_PAGE_ERROR => lx::EIO,
        Foundation::STATUS_ILLEGAL_CHARACTER
        | Foundation::STATUS_INVALID_PARAMETER
        | Foundation::STATUS_INVALID_PARAMETER_1
        | Foundation::STATUS_INVALID_PARAMETER_2
        | Foundation::STATUS_INVALID_PARAMETER_3
        | Foundation::STATUS_INVALID_PARAMETER_4
        | Foundation::STATUS_INVALID_PARAMETER_5
        | Foundation::STATUS_INVALID_PARAMETER_6
        | Foundation::STATUS_INVALID_PARAMETER_7
        | Foundation::STATUS_INVALID_PARAMETER_8
        | Foundation::STATUS_INVALID_PARAMETER_9
        | Foundation::STATUS_INVALID_PARAMETER_10
        | Foundation::STATUS_INVALID_PARAMETER_11
        | Foundation::STATUS_INVALID_PARAMETER_12
        | Foundation::STATUS_OBJECT_PATH_INVALID
        | Foundation::STATUS_INVALID_INFO_CLASS => lx::EINVAL,
        Foundation::STATUS_OBJECT_NAME_INVALID
        | Foundation::STATUS_OBJECT_NAME_NOT_FOUND
        | Foundation::STATUS_OBJECT_PATH_NOT_FOUND
        | Foundation::STATUS_NOT_FOUND
        | Foundation::STATUS_DELETE_PENDING
        | Foundation::STATUS_BAD_NETWORK_NAME
        | Foundation::STATUS_NO_SUCH_FILE => lx::ENOENT,
        Foundation::STATUS_CANNOT_DELETE
        | Foundation::STATUS_INTERNAL_ERROR
        | Foundation::STATUS_WRONG_VOLUME => lx::EIO,
        Foundation::STATUS_TIMEOUT | Foundation::STATUS_IO_TIMEOUT | Foundation::STATUS_RETRY => {
            lx::EAGAIN
        }
        Foundation::STATUS_CANCELLED => lx::EINTR,
        Foundation::STATUS_CONNECTION_DISCONNECTED => lx::EPIPE,
        Foundation::STATUS_CONNECTION_RESET => lx::ECONNRESET,
        Foundation::STATUS_CONNECTION_REFUSED => lx::ECONNREFUSED,
        Foundation::STATUS_BUFFER_TOO_SMALL
        | Foundation::STATUS_BUFFER_OVERFLOW
        | Foundation::STATUS_INFO_LENGTH_MISMATCH => lx::EOVERFLOW,
        Foundation::STATUS_MEDIA_WRITE_PROTECTED => lx::EROFS,
        Foundation::STATUS_EXTERNAL_BACKING_PROVIDER_UNKNOWN
        | Foundation::STATUS_GRAPHICS_ALLOCATION_CONTENT_LOST
        | Foundation::STATUS_GRAPHICS_GPU_EXCEPTION_ON_DEVICE
        | Foundation::STATUS_DEVICE_REMOVED
        | Foundation::STATUS_GRAPHICS_DRIVER_MISMATCH
        | Foundation::STATUS_NO_SUCH_DEVICE => lx::ENODEV,
        Foundation::STATUS_ACCESS_DENIED
        | Foundation::STATUS_SHARING_VIOLATION
        | Foundation::STATUS_FVE_LOCKED_VOLUME => lx::EACCES,
        Foundation::STATUS_CONNECTION_ABORTED => lx::ECONNABORTED,
        Foundation::STATUS_INTEGER_OVERFLOW => lx::EOVERFLOW,
        Foundation::STATUS_NETWORK_UNREACHABLE => lx::ENETUNREACH,
        Foundation::STATUS_HOST_UNREACHABLE => lx::EHOSTUNREACH,
        Foundation::STATUS_NOT_SUPPORTED
        | Foundation::STATUS_PRIVILEGE_NOT_HELD
        | Foundation::STATUS_PRIVILEGED_INSTRUCTION => lx::EPERM,
        Foundation::STATUS_IO_REPARSE_TAG_NOT_HANDLED => lx::ENXIO,
        Foundation::STATUS_MAPPED_FILE_SIZE_ZERO => lx::ENOEXEC,
        Foundation::STATUS_DISK_FULL => lx::ENOSPC,
        Foundation::STATUS_NOT_IMPLEMENTED => lx::ENOSYS,
        Foundation::STATUS_UNEXPECTED_NETWORK_ERROR => lx::ENOTCONN,
        Foundation::STATUS_BAD_NETWORK_PATH => lx::EHOSTDOWN,
        Foundation::STATUS_NO_MEDIA_IN_DEVICE => lx::ENOMEDIUM,
        Foundation::STATUS_UNRECOGNIZED_MEDIA => lx::EMEDIUMTYPE,
        Foundation::STATUS_NO_EAS_ON_FILE | Foundation::STATUS_NO_MORE_EAS => lx::ENODATA,
        Foundation::STATUS_NOT_A_DIRECTORY => lx::ENOTDIR,
        Foundation::STATUS_FILE_IS_A_DIRECTORY => lx::EISDIR,
        Foundation::STATUS_ILLEGAL_INSTRUCTION
        | Foundation::STATUS_INVALID_DEVICE_REQUEST
        | Foundation::STATUS_EAS_NOT_SUPPORTED => lx::ENOTSUP,
        Foundation::STATUS_INVALID_HANDLE
        | Foundation::STATUS_GRAPHICS_ALLOCATION_CLOSED
        | Foundation::STATUS_GRAPHICS_INVALID_ALLOCATION_INSTANCE
        | Foundation::STATUS_GRAPHICS_INVALID_ALLOCATION_HANDLE
        | Foundation::STATUS_GRAPHICS_WRONG_ALLOCATION_DEVICE
        | Foundation::STATUS_OBJECT_PATH_SYNTAX_BAD => lx::EBADF,
        Foundation::STATUS_GRAPHICS_ALLOCATION_BUSY => lx::EINPROGRESS,
        Foundation::STATUS_OBJECT_TYPE_MISMATCH => lx::EPROTOTYPE,
        s => {
            if s.0 >= 0 {
                0
            } else {
                lx::EINVAL
            }
        }
    })
}

fn are_any_attributes_set(attr: &SetAttributes) -> bool {
    attr.size.is_some()
        || attr.mode.is_some()
        || attr.uid.is_some()
        || attr.gid.is_some()
        || !attr.atime.is_omit()
        || !attr.mtime.is_omit()
        || !attr.ctime.is_omit()
}

pub fn apply_attr_overrides(
    state: &super::VolumeState,
    uid: Option<&mut u32>,
    gid: Option<&mut u32>,
    mode: Option<&mut u32>,
) {
    if let Some(uid) = uid {
        *uid = state.options.uid.unwrap_or(*uid);
    }
    if let Some(gid) = gid {
        *gid = state.options.gid.unwrap_or(*gid);
    }
    if let Some(mode) = mode {
        *mode = state
            .options
            .mode
            .map(|hard_coded| override_mode(*mode, hard_coded))
            .unwrap_or(*mode)
    }
}

/// Determine whether or not the set-user-ID and/or set-group-ID bits need to be cleared from the
/// mode. This is something that Linux normally does so this behavior needs to be emulated.
/// If uid/gid/mode have been overridden for all files, the values will also be updated here.
/// TODO: Determine if we should also clear the security.capability xattr.
pub fn set_attr_check_kill_priv(
    handle: &OwnedHandle,
    state: &super::VolumeState,
    attr: &mut SetAttributes,
) -> lx::Result<()> {
    apply_attr_overrides(
        state,
        attr.uid.as_mut(),
        attr.gid.as_mut(),
        attr.mode.as_mut(),
    );

    // We only need to kill privileges if metadata is enabled, mode is not being set, and size or
    // owner are being set. Special case no changes as this is the payload when doing
    // chown(<path>, -1, -1), which in linux will remove the SUID/SGID bits.
    let is_uid = if are_any_attributes_set(attr) {
        attr.uid.is_some()
    } else {
        true
    };

    if !state.options.metadata
        || (attr.mode.is_some() && attr.mode != Some(lx::MODE_INVALID))
        || (attr.size.is_none() && !is_uid && attr.gid.is_none())
    {
        return Ok(());
    }

    let old_attr = state.get_attributes_by_handle(handle)?;

    // If the file has no mode or no set-user-ID or set-group-ID bits, nothing to be done.
    if old_attr.stat.LxFlags & FileSystem::LX_FILE_METADATA_HAS_MODE == 0
        || old_attr.stat.LxMode & (lx::S_ISUID | lx::S_ISGID) == 0
    {
        return Ok(());
    }

    // If not root and the size is changing, clear both set-user-ID and set-group-ID.
    if attr.thread_uid != 0 && attr.size.is_some() {
        attr.mode = Some(old_attr.stat.LxMode & !(lx::S_ISUID | lx::S_ISGID));
        return Ok(());
    }

    // If the uid or gid changed, or this is the special case chown(.., -1, -1), clear set-user-ID.
    if is_uid || attr.gid.is_some() {
        let mut mode = old_attr.stat.LxMode & !lx::S_ISUID;

        // Clear set-group-ID only if the file is group executable.
        if mode & lx::S_IXGRP != 0 {
            mode &= !lx::S_ISGID;
        }

        attr.mode = Some(mode);
    }

    Ok(())
}

/// Set the attributes of a file, checking whether to kill privileges.
pub fn set_attr(
    handle: &OwnedHandle,
    state: &super::VolumeState,
    mut attr: SetAttributes,
) -> lx::Result<()> {
    set_attr_check_kill_priv(handle, state, &mut attr)?;
    set_attr_core(handle, handle, state, &attr)
}

/// Set the attributes of a file (assumes the kill privilege check was already done)
/// N.B. All functions attempting to change attributes should call set_attr_check_kill_priv() first.
pub fn set_attr_core(
    handle: &OwnedHandle,
    truncate_handle: &OwnedHandle,
    state: &super::VolumeState,
    attr: &SetAttributes,
) -> lx::Result<()> {
    if let Some(size) = attr.size {
        fs::truncate(truncate_handle, size as u64)?;
    }

    if state.options.metadata && (attr.mode.is_some() || attr.uid.is_some() || attr.gid.is_some()) {
        let mode = match attr.mode {
            Some(mode) => {
                if mode & lx::S_IFMT == 0 {
                    return Err(lx::Error::EINVAL);
                }

                mode
            }
            None => lx::MODE_INVALID,
        };

        let uid = attr.uid.unwrap_or(lx::UID_INVALID);
        let gid = attr.gid.unwrap_or(lx::GID_INVALID);
        fs::update_lx_attributes(handle, uid, gid, mode)?;
    }

    // Read-only flag update is done with or without metadata.
    if let Some(mode) = attr.mode {
        fs::chmod(handle, mode)?;
    }

    if !attr.atime.is_omit() || !attr.mtime.is_omit() || !attr.ctime.is_omit() {
        let atime = set_time_to_timespec(&attr.atime);
        let mtime = set_time_to_timespec(&attr.mtime);
        let ctime = set_time_to_timespec(&attr.ctime);
        fs::set_file_times(handle, &atime, &mtime, &ctime)?;
    }

    Ok(())
}

/// Returns the permissions required to change attributes.
pub fn permissions_for_set_attr(attr: &SetAttributes, metadata: bool) -> W32Fs::FILE_ACCESS_RIGHTS {
    let mut access = MINIMUM_PERMISSIONS | W32Fs::FILE_WRITE_ATTRIBUTES;

    // Truncate needs write data access.
    if attr.size.is_some() {
        access |= W32Fs::FILE_WRITE_DATA;
    }

    // Changing metadata requires write EA access.
    // N.B. Truncating may need it to kill privileges.
    if metadata
        && (!are_any_attributes_set(attr)
            || attr.mode.is_some()
            || attr.uid.is_some()
            || attr.gid.is_some()
            || (attr.size.is_some() && attr.thread_uid != 0))
    {
        access |= W32Fs::FILE_WRITE_EA;
    }

    access
}

/// Set the EAs on a file.
pub fn set_extended_attr(handle: &OwnedHandle, ea_buffer: &[u8]) -> lx::Result<()> {
    let mut iosb = Default::default();

    // SAFETY: Calling NtSetEaFile as documented.
    unsafe {
        let _ = check_status(FileSystem::NtSetEaFile(
            Foundation::HANDLE(handle.as_raw_handle()),
            &mut iosb,
            ea_buffer.as_ptr() as *mut ffi::c_void,
            ea_buffer.len() as u32,
        ))?;

        Ok(())
    }
}

/// Create a hard link for a file.
pub fn create_link(
    handle: &OwnedHandle,
    link_root: &OwnedHandle,
    new_path: &Path,
) -> lx::Result<()> {
    let new_path: Vec<u16> = new_path.as_os_str().encode_wide().collect();

    // This matches the public definition of FILE_LINK_INFORMATION in ntifs.h, with the variable length payload field
    // at the end removed. u8 arrays are used for fields to remove the need for padding and repr(packed).
    #[expect(non_snake_case)]
    #[repr(C)]
    #[derive(Debug, Clone, Copy, IntoBytes, Immutable, KnownLayout)]
    struct FILE_LINK_INFORMATION {
        ReplaceIfExists: bool,
        pad: [u8; 7],
        RootDirectory: zerocopy::U64<zerocopy::NativeEndian>, // HANDLE
        FileNameLength: zerocopy::U32<zerocopy::NativeEndian>,
        // The next field is the variable length field.
        // FileName: [u16; 1]
    }

    // Default + FromBytes: External APIs require Default (even though the
    // all-zero repr is not a semantically valid default + Immutable + KnownLayout)
    impl Default for FILE_LINK_INFORMATION {
        fn default() -> Self {
            unsafe { mem::zeroed() }
        }
    }

    // TODO: It would be great if we could do this as a static assert, but offset_of is not a const macro.
    assert_eq!(
        std::mem::offset_of!(FileSystem::FILE_LINK_INFORMATION, FileName),
        size_of::<FILE_LINK_INFORMATION>()
    );

    let file_name_length: u32 = new_path
        .as_bytes()
        .len()
        .try_into()
        .map_err(|_| lx::Error::EINVAL)?;

    let link = FILE_LINK_INFORMATION {
        ReplaceIfExists: false,
        RootDirectory: (link_root.as_raw_handle() as u64).into(),
        FileNameLength: file_name_length.into(),
        ..Default::default()
    };

    impl FileInformationClass for HeaderVec<FILE_LINK_INFORMATION, u16, 1> {
        fn file_information_class(&self) -> FileSystem::FILE_INFORMATION_CLASS {
            FileSystem::FileLinkInformation
        }

        fn as_ptr_len(&self) -> (*const u8, usize) {
            // NOTE: HeaderVec guarantees the header and tail are right after another in memory. The number of valid
            //         bytes is described by HeaderVec::total_byte_len().
            (self.as_ptr().cast::<u8>(), self.total_byte_len())
        }

        fn as_ptr_len_mut(&mut self) -> (*mut u8, usize) {
            (self.as_mut_ptr().cast::<u8>(), self.total_byte_len())
        }
    }

    let mut buffer =
        HeaderVec::<FILE_LINK_INFORMATION, u16, 1>::with_capacity(link, new_path.len());
    buffer.extend_tail_from_slice(new_path.as_slice());

    set_information_file(handle, &buffer)?;

    Ok(())
}

/// Create a new ANSI_STRING and return an error if it fails.
pub fn create_ansi_string(value: &lx::LxStr) -> lx::Result<windows::AnsiStringRef<'_>> {
    windows::AnsiStringRef::new(value.as_bytes()).ok_or(lx::Error::EINVAL)
}

// Renames a source file/directory/stream to a different target
// name.

// N.B. The source file handle must be opened for synchronous access.

// N.B. The target name will be escaped if it contains characters that aren't
//      supported by NTFS.
pub fn rename(
    current: &OwnedHandle,
    target_parent: &OwnedHandle,
    target_path: &Path,
    flags: fs::RenameFlags,
) -> lx::Result<()> {
    // If necessary, escape the path, then convert it into a Vec<u16>
    let target_name: Vec<u16> = if flags.escape_name() {
        path::path_from_lx(target_path.as_os_str().as_encoded_bytes())?
            .as_os_str()
            .encode_wide()
            .collect()
    } else {
        target_path.as_os_str().encode_wide().collect()
    };

    // Create the union field of FILE_RENAME_INFORMATION--if POSIX semantics are enabled,
    // we will use the flags field from FILE_RENAME_INFORMATION_EX
    let rename_info_inner = if flags.posix_semantics() {
        // To match POSIX semantics, both POSIX rename and ignoring read-only
        // are required.
        //
        // N.B. It is assumed that if the file system supports POSIX rename, it
        //      supports the ignore read-only attribute flag. Currently, only
        //      NTFS supports either flag.
        FileSystem::FILE_RENAME_INFORMATION_0 {
            Flags: FileSystem::FILE_RENAME_REPLACE_IF_EXISTS
                | FileSystem::FILE_RENAME_POSIX_SEMANTICS
                | FileSystem::FILE_RENAME_IGNORE_READONLY_ATTRIBUTE,
        }
    } else {
        FileSystem::FILE_RENAME_INFORMATION_0 {
            ReplaceIfExists: true,
        }
    };

    // Create our own FILE_RENAME_INFORMATION struct with the FileName field
    // removed so we can use HeaderVec
    //
    // TODO: Figure out a way of partially automating this process for Windows
    // structures, as a lot of this code is copied from create_link
    #[derive(Default, Clone, Copy)]
    #[repr(C)]
    struct FILE_RENAME_INFORMATION {
        anonymous: FileSystem::FILE_RENAME_INFORMATION_0,
        pad: [u8; 4],
        root_directory: zerocopy::U64<zerocopy::NativeEndian>, // HANDLE
        file_name_length: zerocopy::U32<zerocopy::NativeEndian>,
        // FileName
    }

    // Wrapper around FILE_RENAME_INFORMATION to allow compatibility with FileInformationClass trait
    #[expect(non_camel_case_types)]
    #[derive(Default, Clone, Copy)]
    #[repr(transparent)]
    struct FILE_RENAME_INFORMATION_EX(FILE_RENAME_INFORMATION);

    impl FileInformationClass for HeaderVec<FILE_RENAME_INFORMATION, u16, 1> {
        fn file_information_class(&self) -> FileSystem::FILE_INFORMATION_CLASS {
            FileSystem::FileRenameInformation
        }

        fn as_ptr_len(&self) -> (*const u8, usize) {
            // NOTE: HeaderVec guarantees the header and tail are right after another in memory. The number of valid
            //         bytes is described by HeaderVec::total_byte_len().
            (self.as_ptr().cast::<u8>(), self.total_byte_len())
        }

        fn as_ptr_len_mut(&mut self) -> (*mut u8, usize) {
            (self.as_mut_ptr().cast::<u8>(), self.total_byte_len())
        }
    }

    impl FileInformationClass for HeaderVec<FILE_RENAME_INFORMATION_EX, u16, 1> {
        fn file_information_class(&self) -> FileSystem::FILE_INFORMATION_CLASS {
            FileSystem::FileRenameInformationEx
        }

        fn as_ptr_len(&self) -> (*const u8, usize) {
            // NOTE: HeaderVec guarantees the header and tail are right after another in memory. The number of valid
            //         bytes is described by HeaderVec::total_byte_len().
            (self.as_ptr().cast::<u8>(), self.total_byte_len())
        }

        fn as_ptr_len_mut(&mut self) -> (*mut u8, usize) {
            (self.as_mut_ptr().cast::<u8>(), self.total_byte_len())
        }
    }

    assert_eq!(
        std::mem::offset_of!(FileSystem::FILE_RENAME_INFORMATION, FileName),
        size_of::<FILE_RENAME_INFORMATION>()
    );

    let file_name_length: u32 = target_name
        .as_bytes()
        .len()
        .try_into()
        .map_err(|_| lx::Error::EINVAL)?;

    let info = FILE_RENAME_INFORMATION {
        anonymous: rename_info_inner,
        root_directory: (target_parent.as_raw_handle() as u64).into(),
        file_name_length: file_name_length.into(),
        ..Default::default()
    };

    // If POSIX semantics are enabled, we need to wrap the struct in
    // FILE_RENAME_INFORMATION_EX to pass the correct FileInformationClass to
    // set_information_file
    if flags.posix_semantics() {
        let mut buffer = HeaderVec::<FILE_RENAME_INFORMATION_EX, u16, 1>::with_capacity(
            FILE_RENAME_INFORMATION_EX(info),
            target_name.len(),
        );
        buffer.extend_tail_from_slice(target_name.as_slice());
        set_information_file(current, &buffer)
    } else {
        let mut buffer =
            HeaderVec::<FILE_RENAME_INFORMATION, u16, 1>::with_capacity(info, target_name.len());
        buffer.extend_tail_from_slice(target_name.as_slice());
        set_information_file(current, &buffer)
    }
}

/// Convert a `SetTime` struct to a timespec with omit, now, or a value.
fn set_time_to_timespec(time: &SetTime) -> lx::Timespec {
    match time {
        SetTime::Omit => lx::Timespec::omit(),
        SetTime::Set(duration) => duration.into(),
        SetTime::Now => lx::Timespec::now(),
    }
}

/// Convert an i64 (LARGE_INTEGER) NTTIME to an lx::Timespec.
pub fn nt_time_to_timespec(nt_time: i64, absolute_time: bool) -> lx::Timespec {
    let nt_time = if absolute_time {
        if nt_time > LX_POSIX_EPOCH_OFFSET {
            nt_time - LX_POSIX_EPOCH_OFFSET
        } else {
            0
        }
    } else {
        nt_time
    };

    lx::Timespec {
        seconds: (nt_time / LX_UTIL_NT_UNIT_PER_SEC) as usize,
        nanoseconds: ((nt_time % LX_UTIL_NT_UNIT_PER_SEC) * LX_UTIL_NANO_SEC_PER_NT_UNIT) as usize,
    }
}

/// Returns the NT equivalent (100ns units) to time in nanoseconds.
pub fn nanoseconds_to_nt_time(nanoseconds: i64) -> i64 {
    (nanoseconds + LX_UTIL_NANO_SEC_PER_NT_UNIT - 1) / LX_UTIL_NANO_SEC_PER_NT_UNIT
}

/// Return the supplied timespec in equivalent 100ns units. If the unit conversion causes an overflow,
/// the returned value will be the maximum allowed timeout value.
pub fn timespec_to_nt_time(timespec: &lx::Timespec, absolute_time: bool) -> i64 {
    let nt_units = (timespec.seconds as i64)
        .saturating_mul(LX_UTIL_NT_UNIT_PER_SEC)
        .saturating_add(nanoseconds_to_nt_time(timespec.nanoseconds as i64));

    if absolute_time {
        nt_units.saturating_add(LX_POSIX_EPOCH_OFFSET)
    } else {
        nt_units
    }
}

/// Convert a timespec that can optionally use the `UTIME_OMIT` or `UTIME_NOW` values.
pub fn timespec_utime_to_nt_time(timespec: &lx::Timespec, current_time: i64) -> Option<i64> {
    if timespec.nanoseconds == lx::UTIME_NOW {
        Some(current_time)
    } else if timespec.nanoseconds != lx::UTIME_OMIT {
        Some(timespec_to_nt_time(timespec, true))
    } else {
        None
    }
}

// Determines the correct owner and mode of an item based on
// the parent's properties.
// mode and owner gid will be updated only if the parent has the setgit bit set.
pub fn determine_creation_info(
    parent_mode: lx::mode_t,
    parent_gid: lx::gid_t,
    mode: &mut lx::mode_t,
    owner_gid: &mut lx::gid_t,
) {
    if parent_mode & lx::S_ISGID != 0 {
        if lx::s_isdir(*mode) {
            *mode |= lx::S_ISGID;
        }

        *owner_gid = parent_gid;
    }
}

/// Convert a reparse tag into the appropriate file mode
pub fn reparse_tag_to_file_mode(reparse_tag: u32) -> lx::mode_t {
    // We need to redefine some of the tags, in the windows crate they're defined as i32s
    // instead of u32s. In the headers themselves they're just #define directives
    const IO_REPARSE_TAG_LX_SYMLINK: u32 = FileSystem::IO_REPARSE_TAG_LX_SYMLINK as u32;
    const IO_REPARSE_TAG_LX_FIFO: u32 = FileSystem::IO_REPARSE_TAG_LX_FIFO as u32;
    const IO_REPARSE_TAG_LX_CHR: u32 = FileSystem::IO_REPARSE_TAG_LX_CHR as u32;
    const IO_REPARSE_TAG_LX_BLK: u32 = FileSystem::IO_REPARSE_TAG_LX_BLK as u32;

    match reparse_tag {
        W32Ss::IO_REPARSE_TAG_SYMLINK
        | W32Ss::IO_REPARSE_TAG_MOUNT_POINT
        | IO_REPARSE_TAG_LX_SYMLINK => lx::S_IFLNK,
        W32Ss::IO_REPARSE_TAG_AF_UNIX => lx::S_IFSOCK,
        IO_REPARSE_TAG_LX_FIFO => lx::S_IFIFO,
        IO_REPARSE_TAG_LX_CHR => lx::S_IFCHR,
        IO_REPARSE_TAG_LX_BLK => lx::S_IFBLK,
        _ => 0,
    }
}

/// Convert a file mode to the appropriate reparse tag
pub fn file_mode_to_reparse_tag(file_mode: lx::mode_t) -> u32 {
    match file_mode & lx::S_IFMT {
        lx::S_IFIFO => FileSystem::IO_REPARSE_TAG_LX_FIFO as _,
        lx::S_IFSOCK => W32Ss::IO_REPARSE_TAG_AF_UNIX,
        lx::S_IFCHR => FileSystem::IO_REPARSE_TAG_LX_CHR as _,
        lx::S_IFBLK => FileSystem::IO_REPARSE_TAG_LX_BLK as _,
        lx::S_IFLNK => FileSystem::IO_REPARSE_TAG_LX_SYMLINK as _,
        _ => W32Ss::IO_REPARSE_TAG_RESERVED_ZERO as _,
    }
}

/// Convert a reparse tag to the appropriate DT_* file type.
pub fn reparse_tag_to_file_type(reparse_tag: u32) -> u8 {
    // We need to redefine some of the tags, in the windows crate they're defined as i32s
    // instead of u32s. In the headers themselves they're just #define directives
    const IO_REPARSE_TAG_LX_SYMLINK: u32 = FileSystem::IO_REPARSE_TAG_LX_SYMLINK as _;
    const IO_REPARSE_TAG_LX_FIFO: u32 = FileSystem::IO_REPARSE_TAG_LX_FIFO as _;
    const IO_REPARSE_TAG_LX_CHR: u32 = FileSystem::IO_REPARSE_TAG_LX_CHR as _;
    const IO_REPARSE_TAG_LX_BLK: u32 = FileSystem::IO_REPARSE_TAG_LX_BLK as _;

    match reparse_tag {
        W32Ss::IO_REPARSE_TAG_SYMLINK
        | W32Ss::IO_REPARSE_TAG_MOUNT_POINT
        | IO_REPARSE_TAG_LX_SYMLINK => lx::DT_LNK,
        W32Ss::IO_REPARSE_TAG_AF_UNIX => lx::DT_SOCK,
        IO_REPARSE_TAG_LX_FIFO => lx::DT_FIFO,
        IO_REPARSE_TAG_LX_CHR => lx::DT_CHR,
        IO_REPARSE_TAG_LX_BLK => lx::DT_BLK,
        _ => lx::DT_UNK,
    }
}

/// Check if a UnicodeString is "." or ".."
pub fn is_self_relative_unicode_path(path: &windows::UnicodeString) -> bool {
    const DOT: u16 = '.' as u16;
    matches!(path.as_slice(), [DOT] | [DOT, DOT])
}
