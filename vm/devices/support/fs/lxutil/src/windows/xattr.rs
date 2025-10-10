use crate::windows::util;
use core::ffi;
use headervec::HeaderVec;
use std::mem::offset_of;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::OwnedHandle;
use windows::Wdk::Storage::FileSystem;
use windows::Win32::Foundation;
use windows::Win32::System::SystemServices as W32Ss;
use zerocopy::FromBytes;
use zerocopy::KnownLayout;

const LX_UTIL_CASE_SENSITIVE_NAME: &str = "system.wsl_case_sensitive";

const LX_UTIL_XATTR_NAME_PREFIX: &str = "LX.";
const LX_UTIL_XATTR_NAME_PREFIX_LONG: &str = "\0.XL";
const LX_UTIL_XATTR_NAME_PREFIX_LENGTH: usize = LX_UTIL_XATTR_NAME_PREFIX.len();
const LX_UTIL_XATTR_NAME_MAX: usize = u16::MAX as usize - LX_UTIL_XATTR_NAME_PREFIX_LENGTH;

const LX_UTILP_XATTR_QUERY_RESTART_SCAN: u32 = 0x1;
const LX_UTILP_XATTR_QUERY_RETURN_SINGLE_ENTRY: u32 = 0x2;

const LX_UTILP_EA_VALUE_HEADER: char = 'a';
const LX_UTILP_EA_VALUE_HEADER_SIZE: usize = size_of_val(&LX_UTILP_EA_VALUE_HEADER);
const LX_UTILP_MAX_EA_VALUE_SIZE: usize = u16::MAX as usize - LX_UTILP_EA_VALUE_HEADER_SIZE;

/// Check if the given attribute name is the case sensitivity attribute.
fn is_case_sensitive_attribute(name: &str) -> bool {
    name == LX_UTIL_CASE_SENSITIVE_NAME
}

/// Get the value of the case sensitivity attribute.
fn get_case_sensitive(handle: &OwnedHandle) -> lx::Result<bool> {
    let case_info: FileSystem::FILE_CASE_SENSITIVE_INFORMATION =
        util::query_information_file(handle)?;
    Ok(case_info.Flags & W32Ss::FILE_CS_FLAG_CASE_SENSITIVE_DIR != 0)
}

/// Read an extended attribute in the system namespace.
pub fn get_system(handle: &OwnedHandle, name: &str, value: Option<&mut [u8]>) -> lx::Result<usize> {
    if is_case_sensitive_attribute(name) {
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
    } else {
        Err(lx::Error::ENOTSUP)
    }
}

/// Copy the Linux EA attribute prefix and the specified name into the start of the provided buffer.
fn set_name(name: &str, buffer: &mut [u8]) -> lx::Result<usize> {
    let name_bytes = name.as_bytes();
    assert!(name_bytes.len() < LX_UTIL_XATTR_NAME_MAX);

    buffer[..LX_UTIL_XATTR_NAME_PREFIX_LENGTH]
        .copy_from_slice(LX_UTIL_XATTR_NAME_PREFIX.as_bytes());
    buffer[LX_UTIL_XATTR_NAME_PREFIX_LENGTH..LX_UTIL_XATTR_NAME_PREFIX_LENGTH + name_bytes.len()]
        .copy_from_slice(name_bytes);

    Ok(LX_UTIL_XATTR_NAME_PREFIX_LENGTH + name_bytes.len())
}

/// Queries an EA on a file.
fn query_ea(
    handle: &OwnedHandle,
    name: Option<&str>,
    flags: u32,
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
            offset_of!(FileSystem::FILE_GET_EA_INFORMATION, EaName)
                + LX_UTIL_XATTR_NAME_PREFIX_LENGTH
                + name.len()
        ];
        set_name(
            name,
            &mut buffer[offset_of!(FileSystem::FILE_GET_EA_INFORMATION, EaName)..],
        )?;
        Some(buffer)
    } else {
        None
    };

    // Start with a PAGE_SIZE buffer and grow as needed.
    let mut out_buf = vec![0u8; 4096];
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
                if out_buf.len() >= u16::MAX as usize {
                    out_buf.resize(out_buf.len() + 4096, 0);
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
            _ => return Err(lx::Error::ENODATA),
        }
    }
}

/// Read an extended attribute.
pub fn get(handle: &OwnedHandle, name: &str, value: Option<&mut [u8]>) -> lx::Result<usize> {
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

    // SAFETY: Casting from a byte buffer to a struct with no padding.
    let ea_info = unsafe { &*ea.as_ptr().cast::<FileSystem::FILE_FULL_EA_INFORMATION>() };
    assert_eq!(ea_info.NextEntryOffset, 0);

    // Copy out the value if requested.
    let ea_value_len = ea_info.EaValueLength as usize - LX_UTILP_EA_VALUE_HEADER_SIZE;
    if let Some(value) = value {
        if value.len() < ea_value_len {
            return Err(lx::Error::ERANGE);
        }

        let ea_value_start = offset_of!(FileSystem::FILE_FULL_EA_INFORMATION, EaName)
            + ea_info.EaNameLength as usize
            + 1;
        value[..ea_value_len]
            .copy_from_slice(&ea[ea_value_start + 1..ea_value_start + ea_value_len + 1]);
    }

    Ok(ea_value_len)
}
