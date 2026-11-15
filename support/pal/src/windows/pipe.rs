// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::UnicodeString;
use super::chk_status;
use super::dos_to_nt_path;
use super::security::SecurityDescriptor;
use super::status_to_error;
// TODO: Revert this ntapi fallback once windows/windows-sys expose
// NtCreateNamedPipeFile directly.
use ntapi::ntioapi::NtCreateNamedPipeFile;
use pal_event::Event;
use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::mem::zeroed;
use std::os::windows::prelude::*;
use std::path::Path;
use std::ptr::null_mut;
use std::sync::atomic::AtomicPtr;
use std::sync::atomic::Ordering;
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::FILE_CREATE;
use windows_sys::Wdk::Storage::FileSystem::FILE_NON_DIRECTORY_FILE;
use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN;
use windows_sys::Wdk::Storage::FileSystem::FILE_PIPE_BYTE_STREAM_MODE;
use windows_sys::Wdk::Storage::FileSystem::FILE_PIPE_BYTE_STREAM_TYPE;
use windows_sys::Wdk::Storage::FileSystem::FILE_PIPE_CLOSING_STATE;
use windows_sys::Wdk::Storage::FileSystem::FILE_PIPE_CONNECTED_STATE;
use windows_sys::Wdk::Storage::FileSystem::FILE_PIPE_DISCONNECTED_STATE;
use windows_sys::Wdk::Storage::FileSystem::FILE_PIPE_LISTENING_STATE;
use windows_sys::Wdk::Storage::FileSystem::FILE_PIPE_LOCAL_INFORMATION;
use windows_sys::Wdk::Storage::FileSystem::FILE_PIPE_MESSAGE_MODE;
use windows_sys::Wdk::Storage::FileSystem::FILE_PIPE_MESSAGE_TYPE;
use windows_sys::Wdk::Storage::FileSystem::FILE_PIPE_QUEUE_OPERATION;
use windows_sys::Wdk::Storage::FileSystem::FILE_SYNCHRONOUS_IO_NONALERT;
use windows_sys::Wdk::Storage::FileSystem::FilePipeLocalInformation;
use windows_sys::Wdk::Storage::FileSystem::NtFsControlFile;
use windows_sys::Wdk::Storage::FileSystem::NtOpenFile;
use windows_sys::Wdk::Storage::FileSystem::NtQueryInformationFile;
use windows_sys::Win32::Foundation::GENERIC_READ;
use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::Foundation::OBJ_CASE_INSENSITIVE;
use windows_sys::Win32::Foundation::STATUS_NAME_TOO_LONG;
use windows_sys::Win32::Foundation::STATUS_NOT_SUPPORTED;
use windows_sys::Win32::Foundation::STATUS_PENDING;
use windows_sys::Win32::Foundation::STATUS_PIPE_DISCONNECTED;
use windows_sys::Win32::Security::SECURITY_DESCRIPTOR;
use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::FILE_READ_DATA;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_DATA;
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Ioctl::FILE_ANY_ACCESS;
use windows_sys::Win32::System::Ioctl::FILE_DEVICE_NAMED_PIPE;
use windows_sys::Win32::System::Ioctl::METHOD_BUFFERED;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Pipes::DisconnectNamedPipe;
use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows_sys::Win32::System::Pipes::GetNamedPipeHandleStateW;
use windows_sys::Win32::System::Pipes::GetNamedPipeInfo;
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Pipes::PIPE_SERVER_END;
use windows_sys::Win32::System::Pipes::SetNamedPipeHandleState;

/// Creates a pair of pipe files, returning (read, write).
///
/// These files are opened _without_ FILE_FLAG_OVERLAPPED, meaning they are
/// appropriate for passing to another process.
pub fn pair() -> io::Result<(File, File)> {
    // SAFETY: calling API as documented.
    unsafe {
        let mut read = null_mut();
        let mut write = null_mut();
        if CreatePipe(&mut read, &mut write, null_mut(), 0) == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((File::from_raw_handle(read), File::from_raw_handle(write)))
    }
}

fn open_pipe_driver() -> io::Result<OwnedHandle> {
    let mut pathu: UnicodeString = "\\Device\\NamedPipe\\".try_into().expect("string fits");
    let oa = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: null_mut(),
        ObjectName: pathu.as_mut_ptr(),
        Attributes: 0,
        SecurityDescriptor: null_mut(),
        SecurityQualityOfService: null_mut(),
    };
    unsafe {
        let mut iosb = zeroed();
        let mut handle = null_mut();
        chk_status(NtOpenFile(
            &mut handle,
            GENERIC_READ | SYNCHRONIZE,
            &oa,
            &mut iosb,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_SYNCHRONOUS_IO_NONALERT,
        ))?;
        Ok(OwnedHandle::from_raw_handle(handle.cast::<c_void>()))
    }
}

fn pipe_driver_handle() -> io::Result<RawHandle> {
    static PIPE_DRIVER: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
    let mut handle = PIPE_DRIVER.load(Ordering::Relaxed);
    if handle.is_null() {
        let new_handle = open_pipe_driver()?;
        handle = match PIPE_DRIVER.compare_exchange(
            null_mut(),
            new_handle.as_raw_handle(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => new_handle.into_raw_handle(),
            Err(handle) => handle,
        };
    }
    Ok(handle)
}

/// The pipe transfer mode.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PipeMode {
    /// Byte mode
    Byte,
    /// Message mode
    Message,
}

/// The create disposition.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// Create a new pipe path.
    Create,
    /// Create a new instance for an existing pipe path.
    Open,
}

pub fn new_named_pipe(
    path: impl AsRef<Path>,
    access: u32,
    disposition: Disposition,
    mode: PipeMode,
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
        None,
    )
}

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

fn create_named_pipe(
    root: RawHandle,
    path: &Path,
    access: u32,
    disposition: u32,
    overlapped: bool,
    message_mode: bool,
    security_descriptor: Option<&SecurityDescriptor>,
) -> Result<File, io::Error> {
    unsafe {
        let mut pathu = if root.is_null() {
            dos_to_nt_path(path)?
        } else {
            path.try_into()
                .map_err(|_| status_to_error(STATUS_NAME_TOO_LONG))?
        };
        let mut oa = OBJECT_ATTRIBUTES {
            Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: root.cast::<c_void>(),
            ObjectName: pathu.as_mut_ptr(),
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: security_descriptor.map_or(null_mut(), |descriptor| {
                descriptor
                    .as_ptr()
                    .cast::<SECURITY_DESCRIPTOR>()
                    .cast_const()
            }),
            SecurityQualityOfService: null_mut(),
        };

        let mut timeout: i64 = -120 * 10 * 1000 * 1000;
        let mut handle = null_mut();
        let mut iosb = zeroed();
        chk_status(NtCreateNamedPipeFile(
            &mut handle,
            access | SYNCHRONIZE,
            std::ptr::from_mut(&mut oa).cast(),
            &mut iosb,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            disposition,
            if overlapped {
                0
            } else {
                FILE_SYNCHRONOUS_IO_NONALERT
            },
            if message_mode {
                FILE_PIPE_MESSAGE_TYPE
            } else {
                FILE_PIPE_BYTE_STREAM_TYPE
            },
            if message_mode {
                FILE_PIPE_MESSAGE_MODE
            } else {
                FILE_PIPE_BYTE_STREAM_MODE
            },
            FILE_PIPE_QUEUE_OPERATION,
            !0,
            4096,
            4096,
            std::ptr::from_mut(&mut timeout).cast(),
        ))?;
        Ok(File::from_raw_handle(handle.cast::<c_void>()))
    }
}

pub fn bidirectional_pair(message_mode: bool) -> io::Result<(File, File)> {
    unsafe {
        let read_pipe = create_named_pipe(
            pipe_driver_handle()?,
            "".as_ref(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_CREATE,
            false,
            message_mode,
            None,
        )?;

        let mut empty_name = zeroed();
        let oa = OBJECT_ATTRIBUTES {
            Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: read_pipe.as_raw_handle().cast::<c_void>(),
            ObjectName: &mut empty_name,
            Attributes: 0,
            SecurityDescriptor: null_mut(),
            SecurityQualityOfService: null_mut(),
        };
        let mut iosb = zeroed();
        let mut write_pipe_handle = null_mut();
        chk_status(NtOpenFile(
            &mut write_pipe_handle,
            GENERIC_READ | GENERIC_WRITE | SYNCHRONIZE | FILE_READ_ATTRIBUTES,
            &oa,
            &mut iosb,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_SYNCHRONOUS_IO_NONALERT | FILE_NON_DIRECTORY_FILE,
        ))?;
        let write_pipe = File::from_raw_handle(write_pipe_handle.cast::<c_void>());
        Ok((read_pipe, write_pipe))
    }
}

pub trait PipeExt {
    fn get_pipe_state(&self) -> io::Result<u32>;
    fn get_pipe_buffer_sizes(&self) -> io::Result<(u32, u32)>;
    fn set_pipe_mode(&self, mode: u32) -> io::Result<()>;
    fn set_pipe_select_event(
        &self,
        event: &Event,
        event_types: u32,
        control: &mut PipeEventControl,
    ) -> io::Result<()>;
    fn get_pipe_select_events(&self, control: &mut PipeEventControl) -> io::Result<u32>;
    fn is_pipe_connected(&self) -> io::Result<bool>;
    /// Returns whether the pipe's peer is gone, that is, whether the pipe is in
    /// the disconnected or closing state.
    ///
    /// On a server end, this is true once the client closes its handle or the
    /// pipe is disconnected. On a client end, this is true once the server
    /// closes its handle or disconnects the pipe.
    fn is_pipe_peer_closed(&self) -> io::Result<bool>;
    /// Returns the ID of the process at the other end of the pipe.
    ///
    /// On a server end, this is the client process that
    /// `GetNamedPipeClientProcessId` reports. On a client end, where that
    /// function reports the client's own process, this is the server process
    /// that `GetNamedPipeServerProcessId` reports instead. Either way, it is the
    /// process that created or opened the peer end, which may no longer hold it.
    ///
    /// Fails if this is not a pipe or no peer has connected. After the peer
    /// closes its handle, this can still report the peer's process ID, which
    /// Windows can reuse once that process exits.
    fn get_pipe_peer_process_id(&self) -> io::Result<u32>;
    fn disconnect_pipe(&self) -> io::Result<()>;
}

/// Reusable completion state for named-pipe event controls.
///
/// Mutable access serializes controls using this context. Its completion
/// event is private and separate from the selected readiness event.
#[derive(Debug)]
pub struct PipeEventControl {
    completion: Event,
}

impl Default for PipeEventControl {
    fn default() -> Self {
        Self::new()
    }
}

impl PipeEventControl {
    /// Creates a completion context.
    pub fn new() -> Self {
        Self {
            completion: Event::new(),
        }
    }

    /// Issues a pipe control and waits for pending I/O before returning.
    ///
    /// # Safety
    ///
    /// The input and output buffers must be valid for their respective lengths.
    unsafe fn fs_control(
        &mut self,
        file: &File,
        code: u32,
        input: *const c_void,
        input_len: u32,
        output: *mut c_void,
        output_len: u32,
    ) -> i32 {
        let mut iosb = unsafe { zeroed() };
        // A null APC context keeps this internally waited request off any IOCP.
        let status = unsafe {
            NtFsControlFile(
                file.as_raw_handle(),
                self.completion.as_handle().as_raw_handle(),
                None,
                null_mut(),
                &mut iosb,
                code,
                input,
                input_len,
                output,
                output_len,
            )
        };
        if status == STATUS_PENDING {
            self.completion.wait();
            unsafe { iosb.Anonymous.Status }
        } else {
            status
        }
    }
}

impl PipeExt for File {
    fn get_pipe_state(&self) -> io::Result<u32> {
        unsafe {
            let mut state = 0;
            if GetNamedPipeHandleStateW(
                self.as_raw_handle(),
                &mut state,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                0,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(state)
        }
    }

    fn get_pipe_buffer_sizes(&self) -> io::Result<(u32, u32)> {
        let mut flags = 0;
        let mut out_buffer_size = 0;
        let mut in_buffer_size = 0;
        unsafe {
            if GetNamedPipeInfo(
                self.as_raw_handle(),
                &mut flags,
                &mut out_buffer_size,
                &mut in_buffer_size,
                null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        if flags & PIPE_SERVER_END != 0 {
            Ok((in_buffer_size, out_buffer_size))
        } else {
            Ok((out_buffer_size, in_buffer_size))
        }
    }

    fn set_pipe_mode(&self, mode: u32) -> io::Result<()> {
        unsafe {
            if SetNamedPipeHandleState(self.as_raw_handle(), &mode, null_mut(), null_mut()) == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn set_pipe_select_event(
        &self,
        event: &Event,
        event_types: u32,
        control: &mut PipeEventControl,
    ) -> io::Result<()> {
        let mut input = FILE_PIPE_EVENT_SELECT_BUFFER {
            event_types,
            event_handle: event.as_handle().as_raw_handle() as usize as u64,
        };
        unsafe {
            let mut status = !0;
            // Newer versions of Windows support FSCTL_PIPE_EVENT_SELECT, which
            // works on unidirectional pipes. Older versions require
            // FSCTL_PIPE_EVENT_SELECT_OLD, which only works on bidirectional
            // pipes.
            for fsctl in [FSCTL_PIPE_EVENT_SELECT, FSCTL_PIPE_EVENT_SELECT_OLD] {
                status = control.fs_control(
                    self,
                    fsctl,
                    std::ptr::from_mut(&mut input).cast(),
                    size_of_val(&input) as u32,
                    null_mut(),
                    0,
                );
                if status != STATUS_NOT_SUPPORTED {
                    break;
                }
            }
            chk_status(status)?;
        }
        Ok(())
    }

    fn get_pipe_select_events(&self, control: &mut PipeEventControl) -> io::Result<u32> {
        unsafe {
            let mut handle_to_reset: u64 = 0;
            let mut events: u32 = 0;
            chk_status(control.fs_control(
                self,
                FSCTL_PIPE_EVENT_ENUM,
                std::ptr::from_mut(&mut handle_to_reset).cast(),
                size_of_val(&handle_to_reset) as u32,
                std::ptr::from_mut(&mut events).cast(),
                size_of_val(&events) as u32,
            ))?;
            Ok(events)
        }
    }

    fn is_pipe_connected(&self) -> io::Result<bool> {
        // SAFETY: calling with appropriately sized buffer.
        let info = unsafe {
            let mut iosb = zeroed();
            let mut info: FILE_PIPE_LOCAL_INFORMATION = zeroed();
            chk_status(NtQueryInformationFile(
                self.as_raw_handle().cast::<c_void>(),
                &mut iosb,
                std::ptr::from_mut(&mut info).cast(),
                size_of_val(&info) as u32,
                FilePipeLocalInformation,
            ))?;
            info
        };
        let connected = match info.NamedPipeState {
            FILE_PIPE_DISCONNECTED_STATE => false,
            FILE_PIPE_LISTENING_STATE => false,
            FILE_PIPE_CONNECTED_STATE => true,
            FILE_PIPE_CLOSING_STATE => true,
            _ => false,
        };
        Ok(connected)
    }

    fn is_pipe_peer_closed(&self) -> io::Result<bool> {
        // SAFETY: calling with an appropriately sized output buffer.
        let (status, info) = unsafe {
            let mut iosb = zeroed();
            let mut info: FILE_PIPE_LOCAL_INFORMATION = zeroed();
            let status = NtQueryInformationFile(
                self.as_raw_handle().cast::<c_void>(),
                &mut iosb,
                std::ptr::from_mut(&mut info).cast(),
                size_of_val(&info) as u32,
                FilePipeLocalInformation,
            );
            (status, info)
        };
        // Once the server disconnects the pipe, a client end can no longer be
        // queried, but its peer is gone.
        if status == STATUS_PIPE_DISCONNECTED {
            return Ok(true);
        }
        chk_status(status)?;
        Ok(matches!(
            info.NamedPipeState,
            FILE_PIPE_DISCONNECTED_STATE | FILE_PIPE_CLOSING_STATE
        ))
    }

    fn get_pipe_peer_process_id(&self) -> io::Result<u32> {
        let mut flags = 0;
        let mut process_id = 0;
        // SAFETY: the handle is valid for the lifetime of `self` and the output
        // pointers are valid. Windows fails these calls for handles that are
        // not pipes.
        unsafe {
            if GetNamedPipeInfo(
                self.as_raw_handle(),
                &mut flags,
                null_mut(),
                null_mut(),
                null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let succeeded = if flags & PIPE_SERVER_END != 0 {
                GetNamedPipeClientProcessId(self.as_raw_handle(), &mut process_id)
            } else {
                GetNamedPipeServerProcessId(self.as_raw_handle(), &mut process_id)
            };
            if succeeded == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(process_id)
    }

    fn disconnect_pipe(&self) -> io::Result<()> {
        // SAFETY: calling on a known valid handle.
        if unsafe { DisconnectNamedPipe(self.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

pub const FILE_PIPE_READ_READY: u32 = 1;
pub const FILE_PIPE_WRITE_READY: u32 = 2;
pub const FILE_PIPE_DISCONNECTED: u32 = 4;

const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    (device_type << 16) | (access << 14) | (function << 2) | method
}

const FSCTL_PIPE_EVENT_SELECT: u32 = ctl_code(
    FILE_DEVICE_NAMED_PIPE,
    3071,
    METHOD_BUFFERED,
    FILE_ANY_ACCESS,
);
const FSCTL_PIPE_EVENT_SELECT_OLD: u32 = ctl_code(
    FILE_DEVICE_NAMED_PIPE,
    3071,
    METHOD_BUFFERED,
    FILE_WRITE_DATA,
);
const FSCTL_PIPE_EVENT_ENUM: u32 = ctl_code(
    FILE_DEVICE_NAMED_PIPE,
    3072,
    METHOD_BUFFERED,
    FILE_READ_DATA,
);

#[repr(C)]
struct FILE_PIPE_EVENT_SELECT_BUFFER {
    event_types: u32,
    event_handle: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::IoCompletionPort;
    use crate::windows::security::LocalSecurityDescriptor;
    use std::fs::OpenOptions;
    use std::io::Read;
    use std::io::Write;
    use std::process::Child;
    use std::process::Command;
    use std::process::Stdio;
    use std::time::Duration;
    use std::time::Instant;
    use test_with_tracing::test;
    use widestring::U16CStr;
    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
    use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
    use windows_sys::Win32::Foundation::ERROR_PIPE_NOT_CONNECTED;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
    use windows_sys::Win32::Security::Authorization::SDDL_REVISION_1;
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::GetKernelObjectSecurity;
    use windows_sys::Win32::System::Pipes::PIPE_NOWAIT;

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

    /// Tells a child running `pipe_peer_child` which end of which pipe to hold.
    const PEER_CHILD_ENV: &str = "PAL_PIPE_PEER_CHILD";

    fn random_pipe_path() -> String {
        let mut name = [0; 16];
        getrandom::fill(&mut name).unwrap();
        format!(r#"\\.\pipe\{:0x}"#, u128::from_ne_bytes(name))
    }

    /// Opens the client end of `path`, waiting for its server to create it.
    fn open_client(path: &str) -> File {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match OpenOptions::new().read(true).write(true).open(path) {
                Ok(file) => return file,
                Err(err)
                    if Instant::now() < deadline
                        && matches!(
                            err.raw_os_error().map(|code| code as u32),
                            Some(ERROR_FILE_NOT_FOUND | ERROR_PIPE_BUSY)
                        ) =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("failed to open {path}: {err}"),
            }
        }
    }

    /// Runs this test binary as a child process that holds the `role` end of
    /// `path` in `pipe_peer_child`.
    fn spawn_peer_child(role: &str, path: &str) -> Child {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "windows::pipe::tests::pipe_peer_child"])
            .env(PEER_CHILD_ENV, format!("{role}|{path}"))
            .stdout(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn nonblocking_pipe_pair(overlapped: bool) -> (File, File) {
        let path = random_pipe_path();
        let server = create_named_pipe(
            null_mut(),
            Path::new(&path),
            GENERIC_READ | GENERIC_WRITE,
            FILE_CREATE,
            overlapped,
            false,
            None,
        )
        .unwrap();
        let client = open_client(&path);
        server.set_pipe_mode(PIPE_NOWAIT).unwrap();
        (server, client)
    }

    #[test]
    fn pipe_events_follow_pipe_state() {
        let mut control = PipeEventControl::new();
        for overlapped in [false, true] {
            let (mut server, mut client) = nonblocking_pipe_pair(overlapped);
            let event = Event::new();
            server
                .set_pipe_select_event(
                    &event,
                    FILE_PIPE_READ_READY | FILE_PIPE_WRITE_READY | FILE_PIPE_DISCONNECTED,
                    &mut control,
                )
                .unwrap();
            assert_eq!(
                server.get_pipe_select_events(&mut control).unwrap(),
                FILE_PIPE_WRITE_READY
            );

            for _ in 0..16 {
                client.write_all(b"x").unwrap();
                assert_ne!(
                    server.get_pipe_select_events(&mut control).unwrap() & FILE_PIPE_READ_READY,
                    0
                );
                server.read_exact(&mut [0]).unwrap();
                assert_eq!(
                    server.get_pipe_select_events(&mut control).unwrap() & FILE_PIPE_READ_READY,
                    0
                );
            }

            drop(client);
            assert_ne!(
                server.get_pipe_select_events(&mut control).unwrap() & FILE_PIPE_DISCONNECTED,
                0
            );
        }
    }

    #[test]
    fn pipe_events_report_completed_errors() {
        let mut control = PipeEventControl::new();
        for overlapped in [false, true] {
            let (server, client) = nonblocking_pipe_pair(overlapped);
            drop(client);
            assert_eq!(
                server
                    .set_pipe_select_event(&Event::new(), FILE_PIPE_DISCONNECTED, &mut control)
                    .unwrap_err()
                    .raw_os_error(),
                Some(ERROR_PIPE_NOT_CONNECTED as i32)
            );
            assert_eq!(
                server
                    .get_pipe_select_events(&mut control)
                    .unwrap_err()
                    .raw_os_error(),
                Some(ERROR_INVALID_PARAMETER as i32)
            );

            let (server, _client) = nonblocking_pipe_pair(overlapped);
            server
                .set_pipe_select_event(&Event::new(), FILE_PIPE_WRITE_READY, &mut control)
                .unwrap();
            assert_eq!(
                server.get_pipe_select_events(&mut control).unwrap(),
                FILE_PIPE_WRITE_READY
            );
        }
    }

    #[test]
    fn pipe_events_do_not_queue_iocp_completions() {
        let (server, _client) = nonblocking_pipe_pair(true);
        let port = IoCompletionPort::new();
        // SAFETY: the live server handle is opened for overlapped I/O.
        unsafe { port.associate(server.as_raw_handle(), 0).unwrap() };
        let event = Event::new();
        let mut control = PipeEventControl::new();
        server
            .set_pipe_select_event(&event, FILE_PIPE_DISCONNECTED, &mut control)
            .unwrap();
        assert_eq!(server.get_pipe_select_events(&mut control).unwrap(), 0);
        let mut entries = [unsafe { zeroed() }; 1];
        assert_eq!(port.get(&mut entries, Some(Duration::ZERO)), 0);
    }

    #[test]
    fn protected_named_pipe_reports_peer_information() {
        let descriptor: LocalSecurityDescriptor = "D:P(A;;GA;;;OW)".parse().unwrap();
        let path = random_pipe_path();

        let server = new_named_pipe_with_security(
            &path,
            GENERIC_READ | GENERIC_WRITE,
            PipeMode::Byte,
            &descriptor,
        )
        .unwrap();
        assert_eq!(dacl_sddl(&server).unwrap(), "D:P(A;;FA;;;OW)");
        assert!(!server.is_pipe_peer_closed().unwrap());
        assert!(server.get_pipe_peer_process_id().is_err());

        let client = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        assert_eq!(
            server.get_pipe_peer_process_id().unwrap(),
            std::process::id()
        );
        assert_eq!(
            client.get_pipe_peer_process_id().unwrap(),
            std::process::id()
        );
        assert!(!server.is_pipe_peer_closed().unwrap());

        drop(client);
        assert!(server.is_pipe_peer_closed().unwrap());
        server.disconnect_pipe().unwrap();
        assert!(server.is_pipe_peer_closed().unwrap());
        assert!(server.get_pipe_peer_process_id().is_err());
    }

    #[test]
    fn client_end_reports_peer_closed_after_server_disconnects() {
        let path = random_pipe_path();
        let server = new_named_pipe(
            &path,
            GENERIC_READ | GENERIC_WRITE,
            Disposition::Create,
            PipeMode::Byte,
        )
        .unwrap();
        let client = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(!client.is_pipe_peer_closed().unwrap());
        server.disconnect_pipe().unwrap();
        assert!(client.is_pipe_peer_closed().unwrap());
    }

    /// Holds one end of a pipe when `pipe_ends_report_peer_process_id` runs
    /// this binary as its child, and does nothing otherwise.
    #[test]
    fn pipe_peer_child() {
        let Ok(arg) = std::env::var(PEER_CHILD_ENV) else {
            return;
        };
        let (role, path) = arg.split_once('|').unwrap();
        match role {
            // Connect, then exit, which closes the client end.
            "client" => drop(open_client(path)),
            // Serve until the client closes its end.
            "server" => {
                let server = new_named_pipe(
                    path,
                    GENERIC_READ | GENERIC_WRITE,
                    Disposition::Create,
                    PipeMode::Byte,
                )
                .unwrap();
                let deadline = Instant::now() + Duration::from_secs(30);
                while !server.is_pipe_peer_closed().unwrap() {
                    assert!(Instant::now() < deadline, "no client closed {path}");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            _ => panic!("unknown pipe role {role}"),
        }
    }

    #[test]
    fn pipe_ends_report_peer_process_id() {
        // A server end reports the client process, even after it exits.
        let path = random_pipe_path();
        let server = new_named_pipe(
            &path,
            GENERIC_READ | GENERIC_WRITE,
            Disposition::Create,
            PipeMode::Byte,
        )
        .unwrap();
        let mut child = spawn_peer_child("client", &path);
        assert!(child.wait().unwrap().success());
        assert_eq!(server.get_pipe_peer_process_id().unwrap(), child.id());

        // A client end reports the server process.
        let path = random_pipe_path();
        let mut child = spawn_peer_child("server", &path);
        let client = open_client(&path);
        assert_eq!(client.get_pipe_peer_process_id().unwrap(), child.id());
        drop(client);
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn peer_helpers_reject_non_pipes() {
        let file = File::open(std::env::current_exe().unwrap()).unwrap();
        assert!(file.get_pipe_peer_process_id().is_err());
        assert!(file.is_pipe_peer_closed().is_err());
    }
}
