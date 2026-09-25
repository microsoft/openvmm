// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Named-pipe helpers for local peer endpoints: creating a pipe with an
//! explicit security descriptor, identifying the client process, checking
//! whether a file is a pipe, and detecting a closed peer.

use super::Disposition;
use super::PipeMode;
use super::create_named_pipe;
use crate::windows::chk_status;
use crate::windows::security::SecurityDescriptor;
use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::mem::zeroed;
use std::os::windows::prelude::*;
use std::path::Path;
use std::ptr::null_mut;
use windows_sys::Wdk::Storage::FileSystem::FILE_CREATE;
use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN;
use windows_sys::Wdk::Storage::FileSystem::FILE_PIPE_CLOSING_STATE;
use windows_sys::Wdk::Storage::FileSystem::FILE_PIPE_DISCONNECTED_STATE;
use windows_sys::Wdk::Storage::FileSystem::FILE_PIPE_LOCAL_INFORMATION;
use windows_sys::Wdk::Storage::FileSystem::FilePipeLocalInformation;
use windows_sys::Wdk::Storage::FileSystem::NtQueryInformationFile;
use windows_sys::Win32::Storage::FileSystem::FILE_TYPE_PIPE;
use windows_sys::Win32::Storage::FileSystem::GetFileType;
use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;

pub fn new_named_pipe_with_security(
    path: impl AsRef<Path>,
    access: u32,
    disposition: Disposition,
    mode: PipeMode,
    security_descriptor: &SecurityDescriptor,
) -> io::Result<File> {
    create_named_pipe(
        null_mut(),
        path.as_ref(),
        access,
        match disposition {
            Disposition::Create => FILE_CREATE,
            Disposition::Open => FILE_OPEN,
        },
        true,
        mode == PipeMode::Message,
        Some(security_descriptor),
    )
}

/// Returns the `OBJECT_ATTRIBUTES` security descriptor pointer that
/// `create_named_pipe` uses for an optional security descriptor.
pub(super) fn security_descriptor_ptr(
    security_descriptor: Option<&SecurityDescriptor>,
) -> *const windows_sys::Win32::Security::SECURITY_DESCRIPTOR {
    security_descriptor.map_or(null_mut(), |descriptor| {
        descriptor
            .as_ptr()
            .cast::<windows_sys::Win32::Security::SECURITY_DESCRIPTOR>()
            .cast_const()
    })
}

pub fn client_process_id(pipe: &File) -> io::Result<u32> {
    let mut process_id = 0;
    // SAFETY: `pipe` is a live named-pipe handle and the output pointer is
    // valid for the documented call.
    if unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut process_id) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(process_id)
}

pub fn is_pipe(file: &File) -> bool {
    // SAFETY: `file` owns a valid Windows handle.
    unsafe { GetFileType(file.as_raw_handle()) == FILE_TYPE_PIPE }
}

/// Returns whether the peer of a named pipe is gone, that is, whether the pipe
/// is in the disconnected or closing state.
pub(super) fn is_peer_closed(pipe: &File) -> io::Result<bool> {
    // SAFETY: calling with an appropriately sized output buffer.
    let state = unsafe {
        let mut iosb = zeroed();
        let mut info: FILE_PIPE_LOCAL_INFORMATION = zeroed();
        chk_status(NtQueryInformationFile(
            pipe.as_raw_handle().cast::<c_void>(),
            &mut iosb,
            std::ptr::from_mut(&mut info).cast(),
            size_of_val(&info) as u32,
            FilePipeLocalInformation,
        ))?;
        info.NamedPipeState
    };
    Ok(matches!(
        state,
        FILE_PIPE_DISCONNECTED_STATE | FILE_PIPE_CLOSING_STATE
    ))
}
