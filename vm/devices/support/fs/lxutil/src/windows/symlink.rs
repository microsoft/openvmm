// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::windows::path;
use crate::windows::util;
use pal::windows::UnicodeString;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::OwnedHandle;
use windows::Wdk::Storage::FileSystem;
use windows::Win32::Foundation;
use windows::Win32::System::SystemServices as W32Ss;

// TODO: Remove the need for `unsafe` by enlightening this function of the full
// size of the reparse buffer
/// Get the symlink substitute name and flags from the reparse data.
/// # Safety
/// The caller must guarantee that the reparse data buffer is well-formed,
/// with contains a valid wstring of length `length` at the end of the buffer.
unsafe fn get_substitute_name(
    reparse: &FileSystem::REPARSE_DATA_BUFFER,
) -> lx::Result<(&[u16], u32)> {
    // SAFETY: Caller must guarantee that the reparse data buffer is well-formed.
    let (buffer, offset, length, flags) = unsafe {
        match reparse.ReparseTag {
            W32Ss::IO_REPARSE_TAG_SYMLINK => (
                &reparse.Anonymous.SymbolicLinkReparseBuffer.PathBuffer,
                reparse
                    .Anonymous
                    .SymbolicLinkReparseBuffer
                    .SubstituteNameOffset,
                reparse
                    .Anonymous
                    .SymbolicLinkReparseBuffer
                    .SubstituteNameLength,
                reparse.Anonymous.SymbolicLinkReparseBuffer.Flags,
            ),
            W32Ss::IO_REPARSE_TAG_MOUNT_POINT => (
                &reparse.Anonymous.MountPointReparseBuffer.PathBuffer,
                reparse
                    .Anonymous
                    .MountPointReparseBuffer
                    .SubstituteNameOffset,
                reparse
                    .Anonymous
                    .MountPointReparseBuffer
                    .SubstituteNameLength,
                0,
            ),
            _ => return Err(lx::Error::EIO),
        }
    };

    // SAFETY: The validity of the reparse buffer is provided by the caller. If the buffer is valid,
    // the area pointed to by `buffer + offset` is a valid wstring of length `length`, and this operation is safe.
    let substitute_name = unsafe {
        std::slice::from_raw_parts(
            buffer.as_ptr().byte_offset(offset as _),
            (length as usize) / size_of::<u16>(),
        )
    };

    Ok((substitute_name, flags))
}

/// Translates an absolute NT symlink target to an LX path.
fn translate_absolute_target(
    substitute_name: &[u16],
    state: &super::VolumeState,
) -> lx::Result<lx::LxString> {
    if state.options.sandbox || state.options.symlink_root.is_empty() {
        // EPERM is the default return value if no callback is provided
        return Err(lx::Error::EPERM);
    }

    // Convert from UTF-16 slice to String
    if substitute_name.len() < 6 {
        return Err(lx::Error::EIO);
    }
    let name = if substitute_name[substitute_name.len() - 1] == 0 {
        &substitute_name[..substitute_name.len() - 1]
    } else {
        &substitute_name[..substitute_name.len()]
    };
    let name = match String::from_utf16(name) {
        Ok(name) => name,
        Err(_) => return Err(lx::Error::EIO),
    };

    // If the symlink does not start with \??\, it is malformed.
    if !name.starts_with("\\??\\") {
        return Err(lx::Error::EIO);
    }

    // Next must be a drive letter, a colon, and another separator.
    // N.B. Mount-point junctions, which use a volume GUID style path, are not supported.
    let (_, name) = name.split_at(4);
    let mut name_as_chars = name.chars();
    let drive_letter = match name_as_chars.next() {
        Some(val) => val,
        None => return Err(lx::Error::EIO),
    };
    if name_as_chars.next() != Some(':') || name_as_chars.next() != Some('\\') {
        return Err(lx::Error::EIO);
    };
    let drive_letter = match drive_letter {
        'a'..='z' => drive_letter,
        'A'..='Z' => ((drive_letter as u8) - b'A' + b'a') as char,
        _ => return Err(lx::Error::EIO),
    };

    let (_, name) = name.split_at(2);
    let name = name.replace('\\', "/");
    let target = format!("{}{}{}", &state.options.symlink_root, drive_letter, name);

    Ok(target.into())
}

/// Determine the target of an NT symlink. Only relative links are supported.
/// The caller must guarantee that the reparse buffer is well-formed.
pub unsafe fn read_nt_symlink(
    reparse: &FileSystem::REPARSE_DATA_BUFFER,
    state: &super::VolumeState,
) -> lx::Result<lx::LxString> {
    let (substitute_name, flags) = unsafe { get_substitute_name(reparse)? };

    if flags & FileSystem::SYMLINK_FLAG_RELATIVE == 0 {
        translate_absolute_target(substitute_name, state)
    } else {
        let mut name = UnicodeString::new(substitute_name).map_err(|_| lx::Error::EIO)?;

        path::unescape_path(name.as_mut_slice())
    }
}

/// Determine the length of an NT symlink.
/// The caller must guarantee that the reparse buffer is well-formed.
pub unsafe fn read_nt_symlink_length(
    reparse: &FileSystem::REPARSE_DATA_BUFFER,
    state: &super::VolumeState,
) -> lx::Result<u32> {
    // The length is just the target's UTF-8 length.
    // SAFETY: The validity of the reparse buffer is guaranteed by the caller.
    Ok(unsafe { read_nt_symlink(reparse, state) }?.len() as _)
}

pub fn read(link_file: &OwnedHandle) -> lx::Result<lx::LxString> {
    let standard_info: FileSystem::FILE_STANDARD_INFORMATION =
        util::query_information_file(link_file)?;

    if standard_info.EndOfFile > i16::MAX as i64 || standard_info.EndOfFile == 0 {
        return Err(lx::Error::EIO);
    }

    let mut lx_target = vec![0u8; standard_info.EndOfFile as usize];

    let mut iosb = Default::default();
    let byte_offset = 0;

    // SAFETY: Calling Win32 API with valid parameters.
    let status = unsafe {
        FileSystem::NtReadFile(
            Foundation::HANDLE(link_file.as_raw_handle()),
            None,
            None,
            None,
            &mut iosb,
            lx_target.as_mut_ptr().cast(),
            lx_target.capacity() as u32,
            Some(&byte_offset),
            None,
        )
    };

    if status == Foundation::STATUS_INSUFFICIENT_RESOURCES {
        return Err(lx::Error::ENOMEM);
    } else if status != Foundation::STATUS_SUCCESS {
        return Err(lx::Error::EIO);
    }

    Ok(lx::LxString::from_vec(lx_target))
}
