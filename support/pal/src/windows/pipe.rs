// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::chk_status;
use super::dos_to_nt_path;
use super::status_to_error;
use super::UnicodeString;
use namedpipeapi::GetNamedPipeHandleStateW;
use namedpipeapi::SetNamedPipeHandleState;
use ntapi::ntioapi;
use ntapi::ntioapi::FilePipeLocalInformation;
use ntapi::ntioapi::NtQueryInformationFile;
use ntapi::ntioapi::FILE_OPEN;
use ntapi::ntioapi::FILE_PIPE_CLOSING_STATE;
use ntapi::ntioapi::FILE_PIPE_CONNECTED_STATE;
use ntapi::ntioapi::FILE_PIPE_DISCONNECTED_STATE;
use ntapi::ntioapi::FILE_PIPE_LISTENING_STATE;
use ntapi::ntioapi::FILE_PIPE_LOCAL_INFORMATION;
use ntdef::LARGE_INTEGER;
use ntdef::OBJECT_ATTRIBUTES;
use ntdef::OBJ_CASE_INSENSITIVE;
use ntioapi::NtCreateNamedPipeFile;
use ntioapi::NtFsControlFile;
use ntioapi::NtOpenFile;
use ntioapi::FILE_CREATE;
use ntioapi::FILE_NON_DIRECTORY_FILE;
use ntioapi::FILE_PIPE_BYTE_STREAM_MODE;
use ntioapi::FILE_PIPE_BYTE_STREAM_TYPE;
use ntioapi::FILE_PIPE_MESSAGE_MODE;
use ntioapi::FILE_PIPE_MESSAGE_TYPE;
use ntioapi::FILE_PIPE_QUEUE_OPERATION;
use ntioapi::FILE_SYNCHRONOUS_IO_NONALERT;
use pal_event::Event;
use std::fs::File;
use std::io;
use std::mem::zeroed;
use std::os::windows::prelude::*;
use std::path::Path;
use std::ptr::null_mut;
use std::sync::atomic::AtomicPtr;
use std::sync::atomic::Ordering;
use winapi::shared::ntdef;
use winapi::shared::ntstatus::STATUS_NAME_TOO_LONG;
use winapi::um::namedpipeapi;
use winapi::um::namedpipeapi::DisconnectNamedPipe;
use winapi::um::namedpipeapi::GetNamedPipeInfo;
use winapi::um::winbase::PIPE_SERVER_END;
use winapi::um::winioctl;
use winapi::um::winnt;
use winnt::FILE_READ_ATTRIBUTES;
use winnt::FILE_SHARE_READ;
use winnt::FILE_SHARE_WRITE;
use winnt::GENERIC_READ;
use winnt::GENERIC_WRITE;
use winnt::SYNCHRONIZE;

/// Creates a pair of pipe files, returning (read, write).
///
/// These files are opened _without_ FILE_FLAG_OVERLAPPED, meaning they are
/// appropriate for passing to another process.
pub fn pair() -> io::Result<(File, File)> {
    // SAFETY: calling API as documented.
    unsafe {
        let mut read = null_mut();
        let mut write = null_mut();
        if namedpipeapi::CreatePipe(&mut read, &mut write, null_mut(), 0) == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((File::from_raw_handle(read), File::from_raw_handle(write)))
    }
}

fn open_pipe_driver() -> io::Result<OwnedHandle> {
    let mut pathu: UnicodeString = "\\Device\\NamedPipe\\".try_into().expect("string fits");
    let mut oa = OBJECT_ATTRIBUTES {
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
            &mut oa,
            &mut iosb,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_SYNCHRONOUS_IO_NONALERT,
        ))?;
        Ok(OwnedHandle::from_raw_handle(handle))
    }
}

fn pipe_driver_handle() -> io::Result<RawHandle> {
    static PIPE_DRIVER: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(null_mut());
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
    )
}

fn create_named_pipe(
    root: RawHandle,
    path: &Path,
    access: u32,
    disposition: u32,
    overlapped: bool,
    message_mode: bool,
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
            RootDirectory: root,
            ObjectName: pathu.as_mut_ptr(),
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: null_mut(),
            SecurityQualityOfService: null_mut(),
        };

        let mut timeout: i64 = -120 * 10 * 1000 * 1000;
        let mut handle = null_mut();
        let mut iosb = zeroed();
        chk_status(NtCreateNamedPipeFile(
            &mut handle,
            access | SYNCHRONIZE,
            &mut oa,
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
            std::ptr::from_mut::<i64>(&mut timeout).cast::<LARGE_INTEGER>(),
        ))?;
        Ok(File::from_raw_handle(handle))
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
        )?;

        let mut empty_name = zeroed();
        let mut oa = OBJECT_ATTRIBUTES {
            Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: read_pipe.as_raw_handle(),
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
            &mut oa,
            &mut iosb,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_SYNCHRONOUS_IO_NONALERT | FILE_NON_DIRECTORY_FILE,
        ))?;
        let write_pipe = File::from_raw_handle(write_pipe_handle);
        Ok((read_pipe, write_pipe))
    }
}

pub trait PipeExt {
    fn get_pipe_state(&self) -> io::Result<u32>;
    fn get_pipe_buffer_sizes(&self) -> io::Result<(u32, u32)>;
    fn set_pipe_mode(&self, mode: u32) -> io::Result<()>;
    fn set_pipe_select_event(&self, event: &Event, event_types: u32) -> io::Result<()>;
    fn get_pipe_select_events(&self) -> io::Result<u32>;
    fn is_pipe_connected(&self) -> io::Result<bool>;
    fn disconnect_pipe(&self) -> io::Result<()>;
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

    fn set_pipe_mode(&self, mut mode: u32) -> io::Result<()> {
        unsafe {
            if SetNamedPipeHandleState(self.as_raw_handle(), &mut mode, null_mut(), null_mut()) == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn set_pipe_select_event(&self, event: &Event, event_types: u32) -> io::Result<()> {
        let mut input = FILE_PIPE_EVENT_SELECT_BUFFER {
            event_types,
            event_handle: event.as_handle().as_raw_handle() as usize as u64,
        };
        unsafe {
            let mut iosb = zeroed();
            chk_status(NtFsControlFile(
                self.as_raw_handle(),
                null_mut(),
                None,
                null_mut(),
                &mut iosb,
                FSCTL_PIPE_EVENT_SELECT,
                std::ptr::from_mut::<FILE_PIPE_EVENT_SELECT_BUFFER>(&mut input)
                    .cast::<std::ffi::c_void>(),
                size_of_val(&input) as u32,
                null_mut(),
                0,
            ))?;
        }
        Ok(())
    }

    fn get_pipe_select_events(&self) -> io::Result<u32> {
        unsafe {
            let mut handle_to_reset: u64 = 0;
            let mut events: u32 = 0;
            let mut iosb = zeroed();
            chk_status(NtFsControlFile(
                self.as_raw_handle(),
                null_mut(),
                None,
                null_mut(),
                &mut iosb,
                FSCTL_PIPE_EVENT_ENUM,
                std::ptr::from_mut::<u64>(&mut handle_to_reset).cast::<std::ffi::c_void>(),
                size_of_val(&handle_to_reset) as u32,
                std::ptr::from_mut::<u32>(&mut events).cast::<std::ffi::c_void>(),
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
                self.as_raw_handle(),
                &mut iosb,
                std::ptr::from_mut::<FILE_PIPE_LOCAL_INFORMATION>(&mut info).cast(),
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
    winioctl::FILE_DEVICE_NAMED_PIPE,
    3071,
    winioctl::METHOD_BUFFERED,
    winnt::FILE_WRITE_DATA,
);
const FSCTL_PIPE_EVENT_ENUM: u32 = ctl_code(
    winioctl::FILE_DEVICE_NAMED_PIPE,
    3072,
    winioctl::METHOD_BUFFERED,
    winnt::FILE_READ_DATA,
);

#[repr(C)]
#[allow(clippy::upper_case_acronyms)] // C type
struct FILE_PIPE_EVENT_SELECT_BUFFER {
    event_types: u32,
    event_handle: u64,
}
