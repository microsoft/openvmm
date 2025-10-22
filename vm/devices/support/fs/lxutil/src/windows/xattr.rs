// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::windows::util;
use std::mem::{size_of, size_of_val};
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::OwnedHandle;
use windows::Wdk::Storage::FileSystem;
use windows::Wdk::System::SystemServices::PAGE_SIZE;
use windows::Win32::Foundation;
use windows::Win32::System::SystemServices as W32Ss;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unalign};

const LX_UTIL_CASE_SENSITIVE: &str = "system.wsl_case_sensitive";

const LX_UTIL_XATTR_NAME_PREFIX: &str = "LX.";
const LX_UTIL_XATTR_NAME_PREFIX_LENGTH: usize = LX_UTIL_XATTR_NAME_PREFIX.len();
const LX_UTIL_XATTR_NAME_MAX: usize = u16::MAX as usize - LX_UTIL_XATTR_NAME_PREFIX_LENGTH;

const LX_UTILP_XATTR_QUERY_RESTART_SCAN: i32 = 0x1;
const LX_UTILP_XATTR_QUERY_RETURN_SINGLE_ENTRY: i32 = 0x2;

/// Magic header value 'aexl' (little-endian "lxea") used to identify Linux extended attributes
/// stored in Windows EA (Extended Attributes). This helps distinguish Linux xattrs from
/// native Windows EAs.
const LX_UTILP_EA_VALUE_HEADER: u32 =
    ('a' as u32) | ('e' as u32) << 8 | ('x' as u32) << 16 | ('l' as u32) << 24;
const LX_UTILP_EA_VALUE_HEADER_SIZE: usize = size_of_val(&LX_UTILP_EA_VALUE_HEADER);
const LX_UTILP_MAX_EA_VALUE_SIZE: usize = u16::MAX as usize - LX_UTILP_EA_VALUE_HEADER_SIZE;

const LX_XATTR_CREATE: i32 = 0x1;
const LX_XATTR_REPLACE: i32 = 0x2;

pub const LX_UTIL_XATTR_LIST_CASE_SENSITIVE_DIR: i32 = 0x1;

const LX_UTILP_XATTR_NAMESPACE_SECURITY: &str = "SECURITY.";
const LX_UTILP_XATTR_NAMESPACE_TRUSTED: &str = "TRUSTED.";
const LX_UTILP_XATTR_NAMESPACE_USER: &str = "USER.";

/// FILE_GET_EA_INFORMATION structure without the variable-length EaName field.
/// This matches the Windows API structure layout but allows safe zerocopy operations.
#[repr(C, packed)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy)]
struct FileGetEaInformation {
    next_entry_offset: Unalign<u32>,
    ea_name_length: u8,
    // EaName[1]
}

/// FILE_FULL_EA_INFORMATION structure without the variable-length EaName field.
/// This matches the Windows API structure layout but allows safe zerocopy operations.
#[repr(C, packed)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy)]
struct FileFullEaInformation {
    next_entry_offset: Unalign<u32>,
    flags: u8,
    ea_name_length: u8,
    ea_value_length: Unalign<u16>,
    // EaName[1]
}

/// Get the value of the case sensitivity attribute.
fn get_case_sensitive(handle: &OwnedHandle) -> lx::Result<bool> {
    let case_info: FileSystem::FILE_CASE_SENSITIVE_INFORMATION =
        util::query_information_file(handle)?;
    Ok(case_info.Flags & W32Ss::FILE_CS_FLAG_CASE_SENSITIVE_DIR != 0)
}

/// Set the value of the case sensitivity attribute.
fn set_case_sensitive(handle: &OwnedHandle, value: &[u8], flags: i32) -> lx::Result<()> {
    if flags & LX_XATTR_CREATE != 0 {
        // Always treat this attribute as if it exists.
        return Err(lx::Error::EEXIST);
    }

    if value != b"0" && value != b"1" {
        return Err(lx::Error::EINVAL);
    }

    let case_sensitive = value[0] == b'1';

    let case_info = FileSystem::FILE_CASE_SENSITIVE_INFORMATION {
        Flags: if case_sensitive {
            W32Ss::FILE_CS_FLAG_CASE_SENSITIVE_DIR
        } else {
            0
        },
    };
    util::set_information_file(handle, &case_info)
}

/// Read an extended attribute in the system namespace.
pub fn get_system(handle: &OwnedHandle, name: &str, value: Option<&mut [u8]>) -> lx::Result<usize> {
    if name.is_empty() {
        return Err(lx::Error::EINVAL);
    }

    if name != LX_UTIL_CASE_SENSITIVE {
        return Err(lx::Error::ENOTSUP);
    }

    if let Some(value) = value {
        if value.is_empty() {
            return Err(lx::Error::ERANGE);
        }

        if get_case_sensitive(handle)? {
            value[0] = b'1';
        } else {
            value[0] = b'0';
        }
    }

    Ok(1)
}

/// Copy the Linux EA attribute prefix and the specified name into the start of the provided buffer.
fn set_name(name: &str, buffer: &mut [u8]) -> lx::Result<usize> {
    let name_bytes = name.as_bytes();

    if name_bytes.len() >= LX_UTIL_XATTR_NAME_MAX {
        return Err(lx::Error::ERANGE);
    }

    let total_required = LX_UTIL_XATTR_NAME_PREFIX_LENGTH + name_bytes.len() + 1;
    if buffer.len() < total_required {
        return Err(lx::Error::ERANGE);
    }

    buffer[..LX_UTIL_XATTR_NAME_PREFIX_LENGTH]
        .copy_from_slice(LX_UTIL_XATTR_NAME_PREFIX.as_bytes());
    buffer[LX_UTIL_XATTR_NAME_PREFIX_LENGTH..LX_UTIL_XATTR_NAME_PREFIX_LENGTH + name_bytes.len()]
        .copy_from_slice(name_bytes);
    buffer[LX_UTIL_XATTR_NAME_PREFIX_LENGTH + name_bytes.len()] = 0;

    Ok(LX_UTIL_XATTR_NAME_PREFIX_LENGTH + name_bytes.len())
}

/// Queries an EA on a file.
fn query_ea(
    handle: &OwnedHandle,
    name: Option<&str>,
    flags: i32,
    has_more: Option<&mut bool>,
) -> lx::Result<Vec<u8>> {
    // If an EA name is provided, NTFS returns STATUS_BUFFER_OVERFLOW to indicate
    // it didn't fit in the buffer. If no name is provided, that means some entries
    // fit in the buffer, while STATUS_BUFFER_TOO_SMALL indicates none fit.
    let grow_buffer_status = if name.is_some() {
        Foundation::STATUS_BUFFER_OVERFLOW
    } else {
        Foundation::STATUS_BUFFER_TOO_SMALL
    };

    let restart_scan = flags & LX_UTILP_XATTR_QUERY_RESTART_SCAN != 0;
    let return_single_entry = flags & LX_UTILP_XATTR_QUERY_RETURN_SINGLE_ENTRY != 0;

    let get_ea_buf = if let Some(name) = name {
        let mut buffer = vec![
            0u8;
            size_of::<FileGetEaInformation>()
                + LX_UTIL_XATTR_NAME_PREFIX_LENGTH
                + name.len()
                + 1
        ];
        let name_len = set_name(name, &mut buffer[size_of::<FileGetEaInformation>()..])?;

        let header = FileGetEaInformation {
            next_entry_offset: Unalign::new(0),
            ea_name_length: name_len as u8,
        };
        buffer[..size_of::<FileGetEaInformation>()].copy_from_slice(header.as_bytes());
        Some(buffer)
    } else {
        None
    };

    // Start with a PAGE_SIZE buffer and grow as needed.
    let mut out_buf = vec![0u8; PAGE_SIZE as usize];
    loop {
        let mut io_status = Default::default();
        let status = unsafe {
            FileSystem::NtQueryEaFile(
                Foundation::HANDLE(handle.as_raw_handle()),
                &mut io_status,
                out_buf.as_mut_ptr().cast(),
                out_buf.len() as u32,
                return_single_entry,
                get_ea_buf.as_ref().map(|buf| buf.as_ptr().cast()),
                get_ea_buf.as_ref().map_or(0, |buf| buf.len() as u32),
                None,
                restart_scan,
            )
        };

        match status {
            Foundation::STATUS_SUCCESS => {
                return Ok(out_buf);
            }
            s if s == grow_buffer_status => {
                // Grow the buffer and try again.
                if out_buf.len() <= u16::MAX as usize {
                    out_buf.resize(out_buf.len() + PAGE_SIZE as usize, 0);
                } else {
                    // The buffer was already big enough, so something else must be wrong.
                    return Err(lx::Error::EIO);
                }
            }
            Foundation::STATUS_BUFFER_OVERFLOW => {
                // Some entries fit in the buffer, but not all.
                if let Some(has_more) = has_more {
                    *has_more = true;
                }
                return Ok(out_buf);
            }
            status => {
                // The call failed. This does not indicate the attribute doesn't exist,
                // but some other error.
                return Err(util::nt_status_to_lx(status));
            }
        }
    }
}

/// Read an extended attribute.
pub fn get(handle: &OwnedHandle, name: &str, value: Option<&mut [u8]>) -> lx::Result<usize> {
    if name.is_empty() {
        return Err(lx::Error::EINVAL);
    }

    // Because of the prefix, the size limit for names is smaller than normal Linux.
    if name.len() > LX_UTIL_XATTR_NAME_MAX {
        return Err(lx::Error::ERANGE);
    }

    let ea = query_ea(
        handle,
        Some(name),
        LX_UTILP_XATTR_QUERY_RESTART_SCAN | LX_UTILP_XATTR_QUERY_RETURN_SINGLE_ENTRY,
        None,
    )?;

    // Use zerocopy to safely read the EA information
    let ea_info = FileFullEaInformation::read_from_prefix(&ea)
        .map_err(|_| lx::Error::EIO)?
        .0;

    // The attribute doesn't exist.
    if (ea_info.ea_value_length.get() as usize) < LX_UTILP_EA_VALUE_HEADER_SIZE {
        return Err(lx::Error::ENODATA);
    }

    // Copy out the value if requested.
    let ea_value_len = ea_info.ea_value_length.get() as usize - LX_UTILP_EA_VALUE_HEADER_SIZE;
    if let Some(value) = value {
        if value.len() < ea_value_len {
            return Err(lx::Error::ERANGE);
        }

        let ea_value_start = size_of::<FileFullEaInformation>()
            + ea_info.ea_name_length as usize
            + LX_UTILP_EA_VALUE_HEADER_SIZE
            + 1;

        let ea_value_end = ea_value_start + ea_value_len;
        if ea_value_end > ea.len() {
            return Err(lx::Error::EIO);
        }

        value[..ea_value_len].copy_from_slice(&ea[ea_value_start..ea_value_end]);
    }

    Ok(ea_value_len)
}

/// Check if the specified EA exists on the file.
fn check_exists(handle: &OwnedHandle, name: &str) -> lx::Result<bool> {
    let ea = query_ea(
        handle,
        Some(name),
        LX_UTILP_XATTR_QUERY_RESTART_SCAN | LX_UTILP_XATTR_QUERY_RETURN_SINGLE_ENTRY,
        None,
    )?;

    // Use zerocopy to safely read the EA information
    let ea_info = FileFullEaInformation::read_from_prefix(&ea)
        .map_err(|_| lx::Error::EIO)?
        .0;

    // The attribute doesn't exist.
    if (ea_info.ea_value_length.get() as usize) < LX_UTILP_EA_VALUE_HEADER_SIZE {
        return Ok(false);
    }

    Ok(true)
}

/// Set an extended attribute on a file
fn set_ea(handle: &OwnedHandle, buffer: &[u8]) -> lx::Result<()> {
    let mut io_status = Default::default();
    let status = unsafe {
        FileSystem::NtSetEaFile(
            Foundation::HANDLE(handle.as_raw_handle()),
            &mut io_status,
            buffer.as_ptr().cast(),
            buffer.len() as u32,
        )
    };

    if status == Foundation::STATUS_SUCCESS {
        Ok(())
    } else {
        Err(util::nt_status_to_lx(status))
    }
}

/// Sets a linux extended attribute on a file.
pub fn set(handle: &OwnedHandle, name: &str, value: &[u8], flags: i32) -> lx::Result<()> {
    if name.is_empty() {
        return Err(lx::Error::EINVAL);
    }

    if flags != 0 && flags & !(LX_XATTR_CREATE | LX_XATTR_REPLACE) != 0 {
        return Err(lx::Error::EINVAL);
    }

    // Because of the prefix, the size limit for names is smaller than normal Linux.
    if name.len() > LX_UTIL_XATTR_NAME_MAX || value.len() > LX_UTILP_MAX_EA_VALUE_SIZE {
        return Err(lx::Error::ERANGE);
    }

    // If a flag was specified, it's necessary to check if the EA already exists.
    if flags != 0 {
        let exists = check_exists(handle, name)?;
        if (flags & LX_XATTR_CREATE != 0) && exists {
            return Err(lx::Error::EEXIST);
        } else if (flags & LX_XATTR_REPLACE != 0) && !exists {
            return Err(lx::Error::ENODATA);
        }
    }

    let value_size = value.len() + LX_UTILP_EA_VALUE_HEADER_SIZE;
    let mut buffer = vec![
        0u8;
        size_of::<FileFullEaInformation>()
            + LX_UTIL_XATTR_NAME_PREFIX_LENGTH
            + name.len()
            + 1
            + value_size
    ];
    let name_len = set_name(name, &mut buffer[size_of::<FileFullEaInformation>()..])?;

    // Use zerocopy to safely set the EA header
    let ea_info = FileFullEaInformation {
        next_entry_offset: Unalign::new(0),
        flags: 0,
        ea_name_length: name_len as u8,
        ea_value_length: Unalign::new(value_size as u16),
    };
    buffer[..size_of::<FileFullEaInformation>()].copy_from_slice(ea_info.as_bytes());

    // Set the EA value header.
    let ea_value_start = size_of::<FileFullEaInformation>() + name_len + 1;
    buffer[ea_value_start..ea_value_start + 4]
        .copy_from_slice(&LX_UTILP_EA_VALUE_HEADER.to_le_bytes());

    // Copy in the EA value.
    buffer[ea_value_start + 4..ea_value_start + 4 + value.len()].copy_from_slice(value);

    set_ea(handle, &buffer)
}

/// Set a linux extended attribute in the system namespace on a file.
pub fn set_system(handle: &OwnedHandle, name: &str, value: &[u8], flags: i32) -> lx::Result<()> {
    if name == LX_UTIL_CASE_SENSITIVE {
        set_case_sensitive(handle, value, flags)
    } else {
        Err(lx::Error::ENOTSUP)
    }
}

/// Check if the EaName of a specified FILE_FULL_EA_INFORMATION buffer matches the Linux EA prefix and namespaces.
fn is_linux_ea(ea_buf: &[u8]) -> bool {
    if ea_buf.len() < size_of::<FileFullEaInformation>() + LX_UTIL_XATTR_NAME_PREFIX_LENGTH {
        return false;
    }

    let name_start = size_of::<FileFullEaInformation>();
    if &ea_buf[name_start..name_start + LX_UTIL_XATTR_NAME_PREFIX_LENGTH]
        != LX_UTIL_XATTR_NAME_PREFIX.as_bytes()
    {
        return false;
    }

    let name = &ea_buf[name_start + LX_UTIL_XATTR_NAME_PREFIX_LENGTH..];
    name.starts_with(LX_UTILP_XATTR_NAMESPACE_SECURITY.as_bytes())
        || name.starts_with(LX_UTILP_XATTR_NAMESPACE_TRUSTED.as_bytes())
        || name.starts_with(LX_UTILP_XATTR_NAMESPACE_USER.as_bytes())
}

/// List extended attributes on a file.
pub fn list(handle: &OwnedHandle, buffer: Option<&mut [u8]>, flags: i32) -> lx::Result<usize> {
    let mut has_more = true;
    let mut query_flags = LX_UTILP_XATTR_QUERY_RESTART_SCAN;

    let mut eas = Vec::new();
    while has_more {
        let query_result = query_ea(handle, None, query_flags, Some(&mut has_more));

        let ea_buf = match query_result {
            Ok(buf) => buf,
            Err(lx::Error::ENODATA) => {
                // No more EAs.
                break;
            }
            Err(e) => {
                return Err(e);
            }
        };

        // After the first call, don't restart the scan.
        query_flags = 0;

        // Loop through the returned EAs and copy out the Linux ones.
        let mut ea_slice = ea_buf.as_slice();
        loop {
            // Use zerocopy to safely read the EA information
            let ea_info = match FileFullEaInformation::read_from_prefix(ea_slice) {
                Ok((info, _)) => info,
                Err(_) => return Err(lx::Error::EIO),
            };

            if is_linux_ea(ea_slice) {
                // This is a Linux EA. Copy out the downcased name, minus the prefix.
                let name_start =
                    size_of::<FileFullEaInformation>() + LX_UTIL_XATTR_NAME_PREFIX_LENGTH;
                let name_end =
                    name_start + ea_info.ea_name_length as usize - LX_UTIL_XATTR_NAME_PREFIX_LENGTH;
                eas.extend(
                    ea_slice[name_start..name_end]
                        .iter()
                        .map(|c| c.to_ascii_lowercase()),
                );
                eas.push(0);
            }

            let next_entry_offset = ea_info.next_entry_offset.get();
            if next_entry_offset == 0 {
                break;
            }

            if next_entry_offset as usize > ea_slice.len() {
                // Malformed EA list.
                return Err(lx::Error::EIO);
            }

            ea_slice = &ea_slice[next_entry_offset as usize..];
        }
    }

    // Add the case sensitivity attribute if requested.
    if flags & LX_UTIL_XATTR_LIST_CASE_SENSITIVE_DIR != 0 {
        eas.extend_from_slice(LX_UTIL_CASE_SENSITIVE.as_bytes());
        eas.push(0);
    }

    if let Some(buffer) = buffer {
        if buffer.len() < eas.len() {
            return Err(lx::Error::ERANGE);
        }

        buffer[..eas.len()].copy_from_slice(&eas);
    }

    Ok(eas.len())
}

/// Remove an extended attribute from a file.
pub fn remove(handle: &OwnedHandle, name: &str) -> lx::Result<()> {
    if !check_exists(handle, name)? {
        return Err(lx::Error::ENODATA);
    }

    let mut buf =
        vec![
            0u8;
            size_of::<FileFullEaInformation>() + LX_UTIL_XATTR_NAME_PREFIX_LENGTH + name.len() + 1
        ];

    let name_len = set_name(name, &mut buf[size_of::<FileFullEaInformation>()..])?;

    let ea_info = FileFullEaInformation {
        next_entry_offset: Unalign::new(0),
        flags: 0,
        ea_name_length: name_len as u8,
        ea_value_length: Unalign::new(0), // Zero length to remove the attribute
    };
    buf[..size_of::<FileFullEaInformation>()].copy_from_slice(ea_info.as_bytes());

    // To remove an extended attribute, set it with a zero value length.
    set_ea(handle, &buf)
}
