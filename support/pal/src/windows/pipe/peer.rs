// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Named-pipe helpers for local peer endpoints: creating a pipe with an
//! explicit security descriptor, identifying the client process, checking
//! whether a file is a pipe, and detecting a closed peer.

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
use windows_sys::Wdk::Storage::FileSystem::FILE_PIPE_CLOSING_STATE;
use windows_sys::Wdk::Storage::FileSystem::FILE_PIPE_DISCONNECTED_STATE;
use windows_sys::Wdk::Storage::FileSystem::FILE_PIPE_LOCAL_INFORMATION;
use windows_sys::Wdk::Storage::FileSystem::FilePipeLocalInformation;
use windows_sys::Wdk::Storage::FileSystem::NtQueryInformationFile;
use windows_sys::Win32::Storage::FileSystem::FILE_TYPE_PIPE;
use windows_sys::Win32::Storage::FileSystem::GetFileType;
use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;

/// Creates a new named-pipe path with an explicit security descriptor.
///
/// Windows only applies a named pipe's security descriptor when its path is
/// first created, so this does not support opening another instance of an
/// existing pipe.
pub fn new_named_pipe_with_security(
    path: impl AsRef<Path>,
    access: u32,
    mode: PipeMode,
    security_descriptor: &SecurityDescriptor,
) -> io::Result<File> {
    create_named_pipe(
        null_mut(),
        path.as_ref(),
        access,
        FILE_CREATE,
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

#[cfg(test)]
mod tests {
    use super::super::PipeExt;
    use super::*;
    use crate::windows::security::LocalSecurityDescriptor;
    use std::fs::OpenOptions;
    use test_with_tracing::test;
    use widestring::U16CStr;
    use windows_sys::Win32::Foundation::GENERIC_READ;
    use windows_sys::Win32::Foundation::GENERIC_WRITE;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
    use windows_sys::Win32::Security::Authorization::SDDL_REVISION_1;
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::GetKernelObjectSecurity;

    fn dacl_sddl(file: &File) -> io::Result<String> {
        // SAFETY: both API calls use buffers of the reported size, and the
        // returned SDDL string is owned by LocalFree.
        unsafe {
            let mut descriptor_size = 0;
            GetKernelObjectSecurity(
                file.as_raw_handle(),
                DACL_SECURITY_INFORMATION,
                null_mut(),
                0,
                &mut descriptor_size,
            );
            if descriptor_size == 0 {
                return Err(io::Error::last_os_error());
            }

            let mut descriptor = vec![0u8; descriptor_size as usize];
            if GetKernelObjectSecurity(
                file.as_raw_handle(),
                DACL_SECURITY_INFORMATION,
                descriptor.as_mut_ptr().cast(),
                descriptor_size,
                &mut descriptor_size,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }

            let mut sddl = null_mut();
            if ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor.as_mut_ptr().cast(),
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut sddl,
                null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let sddl_string = U16CStr::from_ptr_str(sddl).to_string_lossy();
            LocalFree(sddl.cast());
            Ok(sddl_string)
        }
    }

    #[test]
    fn protected_named_pipe_reports_peer_information() {
        let descriptor: LocalSecurityDescriptor = "D:P(A;;GA;;;OW)".parse().unwrap();
        let mut name = [0; 16];
        getrandom::fill(&mut name).unwrap();
        let path = format!(r#"\\.\pipe\{:0x}"#, u128::from_ne_bytes(name));

        let server = new_named_pipe_with_security(
            &path,
            GENERIC_READ | GENERIC_WRITE,
            PipeMode::Byte,
            &descriptor,
        )
        .unwrap();
        assert_eq!(dacl_sddl(&server).unwrap(), "D:P(A;;FA;;;OW)");
        assert!(is_pipe(&server));
        assert!(!server.is_pipe_peer_closed().unwrap());

        let client = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        assert_eq!(client_process_id(&server).unwrap(), std::process::id());
        assert!(is_pipe(&client));
        assert!(!server.is_pipe_peer_closed().unwrap());

        drop(client);
        assert!(server.is_pipe_peer_closed().unwrap());
    }
}
