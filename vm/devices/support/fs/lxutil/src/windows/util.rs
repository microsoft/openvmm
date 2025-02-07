// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::api;
use crate::SetAttributes;
use crate::SetTime;
use ntapi::ntioapi;
use ntapi::ntrtl::RtlIsDosDeviceName_U;
use pal::windows;
use pal::HeaderVec;
use std::ffi;
use std::mem;
use std::os::windows::prelude::*;
use std::path::Path;
use std::ptr;
use winapi::shared::basetsd;
use winapi::shared::ntdef;
use winapi::shared::ntstatus;
use winapi::um::winioctl;
use winapi::um::winnt;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

// Minimum permissions needed to access a file's metadata.
pub const MINIMUM_PERMISSIONS: u32 =
    winnt::FILE_READ_ATTRIBUTES | winnt::FILE_READ_EA | winnt::READ_CONTROL;

// RAII wrapper around LX_UTIL_DIRECTORY_ENUMERATOR.
pub struct LxUtilDirEnum {
    pub enumerator: api::LX_UTIL_DIRECTORY_ENUMERATOR,
}

impl LxUtilDirEnum {
    // Creates a zeroed-out enumerator.
    pub fn new() -> Self {
        Self {
            enumerator: unsafe { mem::zeroed() },
        }
    }
}

impl Drop for LxUtilDirEnum {
    fn drop(&mut self) {
        unsafe {
            api::LxUtilDirectoryEnumeratorCleanup(&mut self.enumerator);
        }
    }
}

/// A trait that maps a file information struct for calling into `NtSetInformationFile`, `NtQueryInformationFile` and
/// other methods that accept a `FileInformationClass`.
pub trait FileInformationClass: Default {
    /// The [`ntioapi::FILE_INFORMATION_CLASS`] passed to `FileInformationClass` in different NT methods.
    fn file_information_class(&self) -> ntioapi::FILE_INFORMATION_CLASS;

    /// A ptr and len describing this struct to be used for `FileInformation` in different NT methods.
    ///
    /// # Safety
    /// The intention for these methods are to be used directly with the C API. The ptr, len combo returned may access
    /// padding bytes which would be UB in Rust.
    fn as_ptr_len(&self) -> (*const u8, usize);

    /// A prt and len describing this struct to be used for `FileInformation` in different NT methods.
    ///
    /// # Safety
    /// The intention for these methods are to be used directly with the C API. The ptr, len combo returned may access
    /// padding bytes which would be UB in Rust.
    fn as_ptr_len_mut(&mut self) -> (*mut u8, usize);
}

impl FileInformationClass for ntioapi::FILE_BASIC_INFORMATION {
    fn file_information_class(&self) -> ntioapi::FILE_INFORMATION_CLASS {
        ntioapi::FileBasicInformation
    }

    fn as_ptr_len(&self) -> (*const u8, usize) {
        (ptr::from_ref::<Self>(self).cast::<u8>(), size_of::<Self>())
    }

    fn as_ptr_len_mut(&mut self) -> (*mut u8, usize) {
        (ptr::from_mut::<Self>(self).cast::<u8>(), size_of::<Self>())
    }
}

impl FileInformationClass for ntioapi::FILE_CASE_SENSITIVE_INFORMATION {
    fn file_information_class(&self) -> ntioapi::FILE_INFORMATION_CLASS {
        ntioapi::FileCaseSensitiveInformation
    }

    fn as_ptr_len(&self) -> (*const u8, usize) {
        (ptr::from_ref::<Self>(self).cast::<u8>(), size_of::<Self>())
    }

    fn as_ptr_len_mut(&mut self) -> (*mut u8, usize) {
        (ptr::from_mut::<Self>(self).cast::<u8>(), size_of::<Self>())
    }
}

impl FileInformationClass for ntioapi::FILE_ATTRIBUTE_TAG_INFORMATION {
    fn file_information_class(&self) -> ntioapi::FILE_INFORMATION_CLASS {
        ntioapi::FileAttributeTagInformation
    }

    fn as_ptr_len(&self) -> (*const u8, usize) {
        (ptr::from_ref::<Self>(self).cast::<u8>(), size_of::<Self>())
    }

    fn as_ptr_len_mut(&mut self) -> (*mut u8, usize) {
        (ptr::from_mut::<Self>(self).cast::<u8>(), size_of::<Self>())
    }
}

// Open a file using NtCreateFile.
// Returns the create result from the IO_STATUS_BLOCK along with the handle.
pub fn open_relative_file(
    root: Option<&OwnedHandle>,
    path: &Path,
    desired_access: winnt::ACCESS_MASK,
    creation_disposition: ntdef::ULONG,
    file_attributes: ntdef::ULONG,
    create_options: ntdef::ULONG,
    ea_buffer: Option<&[u8]>,
) -> lx::Result<(OwnedHandle, basetsd::ULONG_PTR)> {
    let path = dos_to_nt_path(root, path)?;
    let mut oa = windows::ObjectAttributes::new();
    oa.name(&path).attributes(ntdef::OBJ_CASE_INSENSITIVE);
    if let Some(handle) = root {
        oa.root(handle.as_handle());
    }

    unsafe {
        let mut iosb = mem::zeroed();
        let mut handle = ptr::null_mut();
        let (ea_ptr, ea_len) = if let Some(ea) = ea_buffer {
            (ea.as_ptr() as *mut ffi::c_void, ea.len() as u32)
        } else {
            (ptr::null_mut(), 0)
        };

        let status = ntioapi::NtCreateFile(
            &mut handle,
            desired_access,
            oa.as_ptr(),
            &mut iosb,
            ptr::null_mut(),
            file_attributes,
            winnt::FILE_SHARE_READ | winnt::FILE_SHARE_WRITE | winnt::FILE_SHARE_DELETE,
            creation_disposition,
            create_options,
            ea_ptr,
            ea_len,
        );

        check_status(status)?;
        Ok((OwnedHandle::from_raw_handle(handle), iosb.Information))
    }
}

// Reopen an existing file handle with different permissions.
pub fn reopen_file(
    file: &OwnedHandle,
    desired_access: winnt::ACCESS_MASK,
) -> lx::Result<OwnedHandle> {
    let (handle, _) = open_relative_file(
        Some(file),
        "".as_ref(),
        desired_access,
        ntioapi::FILE_OPEN,
        0,
        ntioapi::FILE_OPEN_REPARSE_POINT,
        None,
    )?;

    Ok(handle)
}

pub struct LxStatInformation {
    pub stat: ntioapi::FILE_STAT_LX_INFORMATION,
    pub symlink_len: Option<u32>,
    pub is_app_execution_alias: bool,
}

// Get file attributes from a file handle.
pub fn get_attributes_by_handle(
    fs_context: &api::LX_UTIL_FS_CONTEXT,
    fs_context_ptr: ntdef::PVOID,
    handle: &OwnedHandle,
) -> lx::Result<LxStatInformation> {
    unsafe {
        let mut stat = mem::zeroed();
        check_lx_error(api::LxUtilFsQueryStatLxInformation(
            handle.as_raw_handle(),
            fs_context,
            &mut stat,
        ))?;

        // For NT symlinks and V2 LX symlinks, the size of the file is not correct, and must be
        // determined based on the reparse data.
        let symlink_len = if is_symlink(stat.FileAttributes, stat.ReparseTag)
            && *stat.EndOfFile.QuadPart() == 0
        {
            let mut symlink_len: ntdef::ULONG = 0;
            api::LxUtilFsReadLinkLength(
                fs_context,
                handle.as_raw_handle(),
                fs_context_ptr,
                &mut symlink_len,
            );
            Some(symlink_len)
        } else {
            None
        };
        let is_app_execution_alias = stat.EndOfFile.QuadPart().eq(&0)
            && api::LxUtilFsIsAppExecLink(stat.FileAttributes, stat.ReparseTag) != 0;

        Ok(LxStatInformation {
            stat,
            symlink_len,
            is_app_execution_alias,
        })
    }
}

// Get file attributes from a file name.
// This may open the file if NtQueryInformationByName is not supported.
pub fn get_attributes(
    fs_context: &api::LX_UTIL_FS_CONTEXT,
    fs_context_ptr: ntdef::PVOID,
    root_handle: Option<&OwnedHandle>,
    path: &Path,
    existing_handle: Option<&OwnedHandle>,
) -> lx::Result<LxStatInformation> {
    if let Some(existing_handle) = existing_handle {
        return get_attributes_by_handle(fs_context, fs_context_ptr, existing_handle);
    }

    // If NtQueryInformationByName is supported, use it.
    if fs_context.CompatibilityFlags & api::FS_CONTEXT_SUPPORTS_QUERY_BY_NAME != 0 {
        let pathu = dos_to_nt_path(root_handle, path)?;
        let root_raw_handle = if let Some(handle) = root_handle {
            handle.as_raw_handle()
        } else {
            ptr::null_mut()
        };

        unsafe {
            let mut stat = mem::zeroed();
            check_lx_error(api::LxUtilFsQueryStatLxInformationByName(
                fs_context,
                root_raw_handle,
                pathu.as_ptr(),
                &mut stat,
            ))?;

            // For NT symlinks and V2 LX symlinks, the size of the file is not correct, and must be
            // determined based on the reparse data, which requires opening the file.
            let symlink_len = if is_symlink(stat.FileAttributes, stat.ReparseTag)
                && *stat.EndOfFile.QuadPart() == 0
            {
                let mut symlink_len: ntdef::ULONG = 0;
                if let Ok((handle, _)) = open_relative_file(
                    root_handle,
                    path,
                    MINIMUM_PERMISSIONS,
                    ntioapi::FILE_OPEN,
                    0,
                    ntioapi::FILE_OPEN_REPARSE_POINT,
                    None,
                ) {
                    api::LxUtilFsReadLinkLength(
                        fs_context,
                        handle.as_raw_handle(),
                        fs_context_ptr,
                        &mut symlink_len,
                    );
                }
                Some(symlink_len)
            } else {
                None
            };
            let is_app_execution_alias = stat.EndOfFile.QuadPart().eq(&0)
                && api::LxUtilFsIsAppExecLink(stat.FileAttributes, stat.ReparseTag) != 0;

            Ok(LxStatInformation {
                stat,
                symlink_len,
                is_app_execution_alias,
            })
        }
    } else {
        // If NtQueryInformationByName is not supported, open the file to query attributes.
        let (handle, _) = open_relative_file(
            root_handle,
            path,
            MINIMUM_PERMISSIONS,
            ntioapi::FILE_OPEN,
            0,
            ntioapi::FILE_OPEN_REPARSE_POINT,
            None,
        )?;

        get_attributes_by_handle(fs_context, fs_context_ptr, &handle)
    }
}

// Determine if a name is a reserved legacy DOS device name
pub fn is_dos_device_name(name: &ffi::OsStr) -> lx::Result<bool> {
    let nameu = widestring::U16CString::from_os_str(name).map_err(|_| lx::Error::EINVAL)?;
    if unsafe { RtlIsDosDeviceName_U(nameu.as_slice_with_nul().as_ptr().cast_mut()) } == 0 {
        Ok(false)
    } else {
        Ok(true)
    }
}

// Determine if a file is symlink based on its reparse tag.
pub fn is_symlink(attributes: ntdef::ULONG, reparse_tag: ntdef::ULONG) -> bool {
    attributes & winnt::FILE_ATTRIBUTE_REPARSE_POINT != 0
        && (reparse_tag == winnt::IO_REPARSE_TAG_SYMLINK
            || reparse_tag == winnt::IO_REPARSE_TAG_MOUNT_POINT
            || reparse_tag == api::IO_REPARSE_TAG_LX_SYMLINK)
}

// Convert an NTSTATUS into an lx::Result.
// Returns the status value on success in case it held a non-error value.
pub fn check_status(status: ntdef::NTSTATUS) -> lx::Result<ntdef::NTSTATUS> {
    if status < 0 {
        Err(nt_status_to_lx(status))
    } else {
        Ok(status)
    }
}

// Same as check_status, but used for read/write operations to handle EBADF and EOF.
pub fn check_status_rw(status: ntdef::NTSTATUS) -> lx::Result<ntdef::NTSTATUS> {
    if status < 0 {
        match status {
            ntstatus::STATUS_ACCESS_DENIED => Err(lx::Error::EBADF),
            ntstatus::STATUS_END_OF_FILE => Ok(0),
            _ => Err(nt_status_to_lx(status)),
        }
    } else {
        Ok(status)
    }
}

// Convert an LX error code to an lx::Result.
pub fn check_lx_error(result: i32) -> lx::Result<()> {
    if result < 0 {
        Err(lx::Error::from_lx(-result))
    } else {
        Ok(())
    }
}

// Checks if a size is negative, and if so returns it as an error; otherwise, returns the positive
// value.
pub fn check_lx_error_size(result: isize) -> lx::Result<usize> {
    if result < 0 {
        Err(lx::Error::from_lx(-result as i32))
    } else {
        Ok(result as usize)
    }
}

// Convert a DOS path to an NT path depending on whether the root is set.
#[allow(clippy::join_absolute_paths)] // https://github.com/rust-lang/rust-clippy/issues/12244
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
pub fn open_flags_to_access_mask(flags: i32) -> winnt::ACCESS_MASK {
    let mut mask = MINIMUM_PERMISSIONS;
    mask |= match flags & lx::O_ACCESS_MASK {
        lx::O_RDONLY => winnt::FILE_GENERIC_READ,
        lx::O_WRONLY => winnt::FILE_GENERIC_WRITE,
        lx::O_RDWR => winnt::FILE_GENERIC_READ | winnt::FILE_GENERIC_WRITE,
        _ => 0,
    };

    if flags & lx::O_APPEND != 0 {
        // Remove the write permission; the generic permission already includes FILE_APPEND_DATA.
        mask &= !winnt::FILE_WRITE_DATA;
    }

    if flags & lx::O_TRUNC != 0 {
        // Truncate on Linux is allowed with O_RDONLY, but requires write access on Windows.
        mask |= winnt::FILE_WRITE_DATA;
    }

    mask
}

// Convert Linux open flags to file attributes and create options.
pub fn open_flags_to_attributes_options(flags: i32) -> (ntdef::ULONG, ntdef::ULONG) {
    let mut file_attributes = 0;
    let mut create_options = 0;

    if flags & lx::O_DIRECTORY != 0 {
        file_attributes |= winnt::FILE_ATTRIBUTE_DIRECTORY;
        create_options |= ntioapi::FILE_DIRECTORY_FILE;
    } else {
        file_attributes |= winnt::FILE_ATTRIBUTE_NORMAL;
    }

    if flags & lx::O_NOFOLLOW != 0 {
        create_options |= ntioapi::FILE_OPEN_REPARSE_POINT;
    }

    (file_attributes, create_options)
}

// Convert Linux open flags to a create disposition.
pub fn open_flags_to_disposition(flags: i32) -> ntdef::ULONG {
    if flags & lx::O_CREAT != 0 {
        if flags & lx::O_EXCL != 0 {
            ntioapi::FILE_CREATE
        } else {
            ntioapi::FILE_OPEN_IF
        }
    } else {
        ntioapi::FILE_OPEN
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
    fs_context: &api::LX_UTIL_FS_CONTEXT,
    information: &mut LxStatInformation,
    options: &crate::LxVolumeOptions,
    block_size: ntdef::ULONG,
) -> lx::Result<lx::Stat> {
    unsafe {
        let mut stat = mem::zeroed();
        check_lx_error(api::LxUtilFsGetLxAttributes(
            fs_context,
            &mut information.stat,
            api::LX_UTIL_FS_CALLER_HAS_TRAVERSE_PRIVILEGE,
            block_size,
            options.default_uid,
            options.default_gid,
            options.umask,
            options.dmask,
            options.fmask,
            &mut stat,
        ))?;

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
            stat.file_size = api::LX_UTIL_PE_HEADER_SIZE as u64;
        }
        Ok(stat)
    }
}

// Create the reparse buffer for an LX symlink.
pub fn create_link_reparse_buffer(target: &lx::LxStr) -> lx::Result<windows::RtlHeapBuffer> {
    let link_target = create_ansi_string(target)?;

    let mut size = 0;
    unsafe {
        let data = api::LxUtilFsCreateLinkReparseBuffer(link_target.as_ref(), &mut size);
        Ok(windows::RtlHeapBuffer::from_raw(
            data.cast::<u8>(),
            size as usize,
        ))
    }
}

// Create the reparse buffer for an NT symlink.
pub fn create_nt_link_reparse_buffer(target: &ffi::OsStr) -> lx::Result<windows::RtlHeapBuffer> {
    let link_target: windows::UnicodeString = target.try_into().map_err(|_| lx::Error::EINVAL)?;

    let mut size = 0;
    unsafe {
        let data = api::LxUtilFsCreateNtLinkReparseBuffer(link_target.as_ptr(), 0, &mut size);
        Ok(windows::RtlHeapBuffer::from_raw(
            data.cast::<u8>(),
            size as usize,
        ))
    }
}

// Applies a reparse point to a file.
pub fn set_reparse_point(handle: &OwnedHandle, reparse_buffer: &[u8]) -> lx::Result<()> {
    unsafe {
        let mut iosb = mem::zeroed();
        check_status(ntioapi::NtFsControlFile(
            handle.as_raw_handle(),
            ptr::null_mut(),
            None,
            ptr::null_mut(),
            &mut iosb,
            winioctl::FSCTL_SET_REPARSE_POINT,
            reparse_buffer.as_ptr() as *mut ffi::c_void,
            reparse_buffer
                .len()
                .try_into()
                .map_err(|_| lx::Error::EINVAL)?,
            ptr::null_mut(),
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

    // SAFETY: Calling NtSetInformationFile as documented.
    unsafe {
        check_status(ntioapi::NtQueryInformationFile(
            handle.as_raw_handle(),
            &mut iosb,
            buf as ntdef::PVOID,
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
        check_status(ntioapi::NtSetInformationFile(
            handle.as_raw_handle(),
            &mut iosb,
            buf as ntdef::PVOID,
            len.try_into().unwrap(),
            info.file_information_class(),
        ))?;

        Ok(())
    }
}

// Convert an NTSTATUS to a Linux error code.
pub fn nt_status_to_lx(status: ntdef::NTSTATUS) -> lx::Error {
    let mut error = -lx::EINVAL;
    unsafe { api::LxUtilNtStatusToLxError(status, &mut error) };
    lx::Error::from_lx(-error)
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
    if old_attr.stat.LxFlags & api::LX_FILE_METADATA_HAS_MODE == 0
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
    unsafe {
        if let Some(size) = attr.size {
            check_lx_error(api::LxUtilFsTruncate(
                truncate_handle.as_raw_handle(),
                size as u64,
            ))?;
        }

        if state.options.metadata
            && (attr.mode.is_some() || attr.uid.is_some() || attr.gid.is_some())
        {
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
            check_lx_error(api::LxUtilFsUpdateLxAttributes(
                handle.as_raw_handle(),
                uid,
                gid,
                mode,
            ))?;
        }

        // Read-only flag update is done with or without metadata.
        if let Some(mode) = attr.mode {
            check_lx_error(api::LxUtilFsChmod(handle.as_raw_handle(), mode))?;
        }

        if !attr.atime.is_omit() || !attr.mtime.is_omit() || !attr.ctime.is_omit() {
            let atime = set_time_to_timespec(&attr.atime);
            let mtime = set_time_to_timespec(&attr.mtime);
            let ctime = set_time_to_timespec(&attr.ctime);
            check_lx_error(api::LxUtilFsSetTimes(
                handle.as_raw_handle(),
                &atime,
                &mtime,
                &ctime,
            ))?;
        }

        Ok(())
    }
}

/// Returns the permissions required to change attributes.
pub fn permissions_for_set_attr(attr: &SetAttributes, metadata: bool) -> winnt::ACCESS_MASK {
    let mut access = MINIMUM_PERMISSIONS | winnt::FILE_WRITE_ATTRIBUTES;

    // Truncate needs write data access.
    if attr.size.is_some() {
        access |= winnt::FILE_WRITE_DATA;
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
        access |= winnt::FILE_WRITE_EA;
    }

    access
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
    #[allow(non_snake_case)]
    #[repr(C)]
    #[derive(Debug, Clone, Copy, IntoBytes, Immutable, KnownLayout, FromBytes)]
    struct FILE_LINK_INFORMATION {
        ReplaceIfExists: ntdef::BOOLEAN,
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
            Self::new_zeroed()
        }
    }

    // TODO: It would be great if we could do this as a static assert, but offset_of is not a const macro.
    assert_eq!(
        std::mem::offset_of!(ntioapi::FILE_LINK_INFORMATION, FileName),
        size_of::<FILE_LINK_INFORMATION>()
    );

    let file_name_length: u32 = new_path
        .as_bytes()
        .len()
        .try_into()
        .map_err(|_| lx::Error::EINVAL)?;

    let link = FILE_LINK_INFORMATION {
        ReplaceIfExists: ntdef::FALSE,
        RootDirectory: (link_root.as_raw_handle() as u64).into(),
        FileNameLength: file_name_length.into(),
        ..Default::default()
    };

    impl FileInformationClass for HeaderVec<FILE_LINK_INFORMATION, [u16; 1]> {
        fn file_information_class(&self) -> ntioapi::FILE_INFORMATION_CLASS {
            ntioapi::FileLinkInformation
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
        HeaderVec::<FILE_LINK_INFORMATION, [u16; 1]>::with_capacity(link, new_path.len());
    buffer.extend_from_slice(new_path.as_slice());

    set_information_file(handle, &buffer)?;

    Ok(())
}

/// Create a new ANSI_STRING and return an error if it fails.
pub fn create_ansi_string(value: &lx::LxStr) -> lx::Result<windows::AnsiStringRef<'_>> {
    windows::AnsiStringRef::new(value.as_bytes()).ok_or(lx::Error::EINVAL)
}

/// Safe wrapper around LxUtilFsRename.
pub fn rename(
    current: &OwnedHandle,
    target_parent: &OwnedHandle,
    target_name: &windows::UnicodeString,
    fs_context: &api::LX_UTIL_FS_CONTEXT,
) -> lx::Result<()> {
    unsafe {
        check_lx_error(api::LxUtilFsRename(
            current.as_raw_handle(),
            target_parent.as_raw_handle(),
            target_name.as_ptr(),
            fs_context,
            0,
        ))
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
