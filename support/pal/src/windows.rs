// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(windows)]
// UNSAFETY: Calls to Win32 functions to handle delay loading, interacting
// with low level primitives, and memory management.
#![expect(unsafe_code)]
#![expect(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]

pub mod afd;
pub mod affinity;
pub mod alpc;
pub mod fs;
pub mod job;
pub mod pipe;
pub mod process;
pub mod security;
pub mod tp;

use self::security::SecurityDescriptor;
use handleapi::INVALID_HANDLE_VALUE;
use ntapi::ntioapi::FILE_COMPLETION_INFORMATION;
use ntapi::ntioapi::FileReplaceCompletionInformation;
use ntapi::ntioapi::IO_STATUS_BLOCK;
use ntapi::ntioapi::NtAssociateWaitCompletionPacket;
use ntapi::ntioapi::NtCancelWaitCompletionPacket;
use ntapi::ntioapi::NtCreateWaitCompletionPacket;
use ntapi::ntioapi::NtSetInformationFile;
use ntapi::ntobapi::NtCreateDirectoryObject;
use ntapi::ntobapi::NtOpenDirectoryObject;
use ntapi::ntrtl;
use ntdef::ANSI_STRING;
use ntdef::UNICODE_STRING;
use ntrtl::RtlAllocateHeap;
use ntrtl::RtlDosPathNameToNtPathName_U_WithStatus;
use ntrtl::RtlFreeUnicodeString;
use ntrtl::RtlNtStatusToDosErrorNoTeb;
use processthreadsapi::GetExitCodeProcess;
use std::cell::UnsafeCell;
use std::ffi::OsStr;
use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::io::Error;
use std::io::Result;
use std::marker::PhantomData;
use std::mem::zeroed;
use std::os::windows::prelude::*;
use std::path::Path;
use std::ptr::NonNull;
use std::ptr::addr_of;
use std::ptr::null_mut;
use std::sync::Once;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use widestring::U16CString;
use widestring::Utf16Str;
use winapi::shared::ntdef;
use winapi::shared::ntdef::NTSTATUS;
use winapi::shared::ntstatus;
use winapi::shared::ntstatus::STATUS_PENDING;
use winapi::shared::winerror::ERROR_BAD_PATHNAME;
use winapi::shared::ws2def;
use winapi::um::errhandlingapi::GetErrorMode;
use winapi::um::errhandlingapi::SetErrorMode;
use winapi::um::handleapi;
use winapi::um::handleapi::CloseHandle;
use winapi::um::heapapi::GetProcessHeap;
use winapi::um::ioapiset::CreateIoCompletionPort;
use winapi::um::ioapiset::GetQueuedCompletionStatusEx;
use winapi::um::ioapiset::PostQueuedCompletionStatus;
use winapi::um::minwinbase::OVERLAPPED;
use winapi::um::minwinbase::OVERLAPPED_ENTRY;
use winapi::um::processenv::SetStdHandle;
use winapi::um::processthreadsapi;
use winapi::um::processthreadsapi::TerminateProcess;
use winapi::um::synchapi;
use winapi::um::winbase::INFINITE;
use winapi::um::winbase::SEM_FAILCRITICALERRORS;
use winapi::um::winbase::STD_OUTPUT_HANDLE;
use winapi::um::winbase::SetFileCompletionNotificationModes;
use winapi::um::winnt;
use winapi::um::winsock2;

#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct SendSyncRawHandle(pub RawHandle);

unsafe impl Send for SendSyncRawHandle {}
unsafe impl Sync for SendSyncRawHandle {}

pub trait BorrowedHandleExt: Sized {
    fn duplicate(&self, inherit: bool, access: Option<u32>) -> Result<OwnedHandle>;
}

impl BorrowedHandleExt for BorrowedHandle<'_> {
    fn duplicate(&self, inherit: bool, access: Option<u32>) -> Result<OwnedHandle> {
        let mut handle = null_mut();
        let options = if access.is_some() {
            0
        } else {
            winnt::DUPLICATE_SAME_ACCESS
        };
        unsafe {
            let process = processthreadsapi::GetCurrentProcess();
            if handleapi::DuplicateHandle(
                process,
                self.as_raw_handle(),
                process,
                &mut handle,
                access.unwrap_or(0),
                inherit.into(),
                options,
            ) == 0
            {
                return Err(Error::last_os_error());
            }
            Ok(OwnedHandle::from_raw_handle(handle))
        }
    }
}

pub trait OwnedSocketExt: Sized {
    /// Prepares the socket for being sent to another process.
    ///
    /// After calling this, the socket should not be used for anything other
    /// than duplicating to a handle (which can then be converted back to a
    /// socket with `from_handle`).
    fn prepare_to_send(&mut self) -> Result<BorrowedHandle<'_>>;

    /// Converts a handle, originally duplicated from another socket, to a
    /// socket. The original socket should have been prepared with
    /// [`Self::prepare_to_send`].
    fn from_handle(handle: OwnedHandle) -> Result<Self>;
}

const SIO_SOCKET_TRANSFER_BEGIN: u32 = ws2def::IOC_IN | ws2def::IOC_VENDOR | 301;
const SIO_SOCKET_TRANSFER_END: u32 = ws2def::IOC_IN | ws2def::IOC_VENDOR | 302;

/// Ensures WSAStartup has been called for the process.
fn init_winsock() {
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        // Initialize a dummy socket, then throw away the result, to get the
        // socket library to call WSAStartup for us.
        let _ = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None);
    });
}

impl OwnedSocketExt for OwnedSocket {
    fn prepare_to_send(&mut self) -> Result<BorrowedHandle<'_>> {
        let mut catalog_id: u32 = 0;
        let mut bytes = 0;
        // SAFETY: calling the ioctl according to implementation requirements
        unsafe {
            if winsock2::WSAIoctl(
                self.as_raw_socket() as _,
                SIO_SOCKET_TRANSFER_BEGIN,
                null_mut(),
                0,
                std::ptr::from_mut(&mut catalog_id).cast(),
                size_of_val(&catalog_id) as u32,
                &mut bytes,
                null_mut(),
                None,
            ) != 0
            {
                return Err(Error::from_raw_os_error(winsock2::WSAGetLastError()));
            }
            Ok(BorrowedHandle::borrow_raw(self.as_raw_socket() as RawHandle))
        }
    }

    fn from_handle(handle: OwnedHandle) -> Result<Self> {
        // This could be the first winsock interaction for the process.
        init_winsock();

        let mut catalog_id: u32 = 0;
        let mut bytes = 0;
        let mut socket = handle.as_raw_handle() as winsock2::SOCKET;
        // SAFETY: calling the ioctl according to implementation requirements
        unsafe {
            if winsock2::WSAIoctl(
                socket,
                SIO_SOCKET_TRANSFER_END,
                std::ptr::from_mut(&mut catalog_id).cast(),
                size_of_val(&catalog_id) as u32,
                std::ptr::from_mut(&mut socket).cast(),
                size_of_val(&socket) as u32,
                &mut bytes,
                null_mut(),
                None,
            ) != 0
            {
                return Err(Error::from_raw_os_error(winsock2::WSAGetLastError()));
            }
            // In theory SIO_SOCKET_TRANSFER_END could have changed `socket`, so
            // forget the handle and use the socket instead.
            let _gone = handle.into_raw_handle();
            Ok(Self::from_raw_socket(socket as RawSocket))
        }
    }
}

#[repr(transparent)]
#[derive(Debug)]
struct WaitObject(OwnedHandle);

impl WaitObject {
    fn wait(&self) {
        assert!(unsafe { synchapi::WaitForSingleObject(self.0.as_raw_handle(), INFINITE) } == 0);
    }
}

impl Clone for WaitObject {
    fn clone(&self) -> Self {
        Self(
            self.0
                .try_clone()
                .expect("out of resources cloning wait object"),
        )
    }
}

#[derive(Debug, Clone)]
pub struct Process(WaitObject);

impl Process {
    pub fn wait(&self) {
        self.0.wait()
    }

    pub fn id(&self) -> u32 {
        unsafe {
            let pid = processthreadsapi::GetProcessId(self.as_handle().as_raw_handle());
            assert_ne!(pid, 0);
            pid
        }
    }

    pub fn exit_code(&self) -> u32 {
        let mut code = 0;
        unsafe {
            assert!(GetExitCodeProcess(self.as_handle().as_raw_handle(), &mut code) != 0);
        }
        code
    }

    /// Terminates the process immediately, setting its exit code to `exit_code`.
    pub fn kill(&self, exit_code: u32) -> Result<()> {
        // SAFETY: calling TerminateProcess according to API docs.
        unsafe {
            if TerminateProcess(self.as_handle().as_raw_handle(), exit_code) == 0 {
                return Err(Error::last_os_error());
            }
        }
        Ok(())
    }
}

impl From<OwnedHandle> for Process {
    fn from(handle: OwnedHandle) -> Self {
        Self(WaitObject(handle))
    }
}

impl AsHandle for Process {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        (self.0).0.as_handle()
    }
}

impl From<Process> for OwnedHandle {
    fn from(value: Process) -> OwnedHandle {
        (value.0).0
    }
}

#[derive(Debug)]
pub struct IoCompletionPort(OwnedHandle);

impl IoCompletionPort {
    pub fn new() -> Self {
        unsafe {
            let handle = CreateIoCompletionPort(INVALID_HANDLE_VALUE, null_mut(), 0, 0);
            if handle.is_null() {
                panic!("oom allocating completion port");
            }
            Self(OwnedHandle::from_raw_handle(handle))
        }
    }

    pub fn get(&self, entries: &mut [OVERLAPPED_ENTRY], timeout: Option<Duration>) -> usize {
        unsafe {
            let mut n = 0;
            if GetQueuedCompletionStatusEx(
                self.0.as_raw_handle(),
                entries.as_mut_ptr(),
                entries.len().try_into().expect("too many entries"),
                &mut n,
                timeout
                    .map(|t| t.as_millis().try_into().unwrap_or(INFINITE - 1))
                    .unwrap_or(INFINITE),
                false.into(),
            ) != 0
            {
                n as usize
            } else {
                // TODO: assert timeout
                assert!(timeout.is_some());
                0
            }
        }
    }

    // Per MSDN, overlapped values are not dereferenced by PostQueuedCompletionStatus,
    // they are passed as-is to the caller of GetQueuedCompletionStatus.
    #[expect(clippy::not_unsafe_ptr_arg_deref)]
    pub fn post(&self, bytes: u32, key: usize, overlapped: *mut OVERLAPPED) {
        unsafe {
            if PostQueuedCompletionStatus(self.0.as_raw_handle(), bytes, key, overlapped) == 0 {
                panic!("oom posting completion port");
            }
        }
    }

    /// # Safety
    ///
    /// The caller must ensure that `handle` is valid.
    pub unsafe fn associate(&self, handle: RawHandle, key: usize) -> Result<()> {
        if unsafe { CreateIoCompletionPort(handle, self.0.as_raw_handle(), key, 0).is_null() } {
            return Err(Error::last_os_error());
        }
        Ok(())
    }
}

impl From<OwnedHandle> for IoCompletionPort {
    fn from(handle: OwnedHandle) -> Self {
        Self(handle)
    }
}

impl AsHandle for IoCompletionPort {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.0.as_handle()
    }
}

impl From<IoCompletionPort> for OwnedHandle {
    fn from(value: IoCompletionPort) -> OwnedHandle {
        value.0
    }
}

/// Sets file completion notification modes for `handle`.
///
/// # Safety
/// The caller must ensure that `handle` is valid and that changing the
/// notification modes does not cause a safety issue elsewhere (e.g. by causing
/// unexpected IO completions in a completion port).
pub unsafe fn set_file_completion_notification_modes(handle: RawHandle, flags: u8) -> Result<()> {
    // SAFETY: caller guarantees contract.
    if unsafe { SetFileCompletionNotificationModes(handle, flags) } == 0 {
        return Err(Error::last_os_error());
    }
    Ok(())
}

/// Disassociates `handle` from its completion port.
///
/// # Safety
///
/// The caller must ensure that `handle` is valid.
pub unsafe fn disassociate_completion_port(handle: RawHandle) -> Result<()> {
    let mut info = FILE_COMPLETION_INFORMATION {
        Port: null_mut(),
        Key: null_mut(),
    };
    let mut iosb = IO_STATUS_BLOCK::default();
    // SAFETY: caller guarantees contract.
    unsafe {
        chk_status(NtSetInformationFile(
            handle,
            &mut iosb,
            std::ptr::from_mut::<FILE_COMPLETION_INFORMATION>(&mut info).cast(),
            size_of_val(&info) as u32,
            FileReplaceCompletionInformation,
        ))?;
    }
    Ok(())
}

/// Wrapper around an NT IO completion packet, used to deliver wait results to
/// IO completion ports.
#[derive(Debug)]
pub struct WaitPacket(OwnedHandle);

impl WaitPacket {
    /// Creates a new wait copmletion packet.
    pub fn new() -> Result<Self> {
        unsafe {
            let mut handle = null_mut();
            chk_status(NtCreateWaitCompletionPacket(&mut handle, 1, null_mut()))?;
            Ok(Self(OwnedHandle::from_raw_handle(handle)))
        }
    }

    /// Initiates a wait on `handle`. When `handle` becomes signaled, the packet
    /// information will be delivered via `iocp`. Returns true if the handle was
    /// already signaled (in which case the packet will still be delivered
    /// through the IOCP).
    ///
    /// Panics if the wait could not be associated (e.g. invalid handle or wait
    /// already in progress).
    ///
    /// # Safety
    ///
    /// The caller must ensure that `handle` is valid.
    pub unsafe fn associate(
        &self,
        iocp: &IoCompletionPort,
        handle: RawHandle,
        key: usize,
        apc: usize,
        status: i32,
        information: usize,
    ) -> bool {
        // SAFETY: API is being used as documented, and handle is valid
        unsafe {
            let mut already_signaled = 0;
            chk_status(NtAssociateWaitCompletionPacket(
                self.0.as_raw_handle(),
                iocp.as_handle().as_raw_handle(),
                handle,
                key as *mut c_void,
                apc as *mut c_void,
                status,
                information,
                &mut already_signaled,
            ))
            .expect("failed to associate wait completion packet");
            already_signaled != 0
        }
    }

    /// Cancels a pending wait. Returns true if the wait was successfully
    /// cancelled. If `remove_signaled_packet`, then the packet will be removed
    /// from the IOCP (in which case it may have already consumed the signal
    /// state of the object that was being waited upon).
    pub fn cancel(&self, remove_signaled_packet: bool) -> bool {
        match unsafe {
            NtCancelWaitCompletionPacket(self.0.as_raw_handle(), remove_signaled_packet.into())
        } {
            ntstatus::STATUS_SUCCESS => true,
            STATUS_PENDING => false,
            ntstatus::STATUS_CANCELLED => false,
            s => panic!(
                "unexpected failure in NtCancelWaitCompletionPacket: {:?}",
                chk_status(s).unwrap_err()
            ),
        }
    }
}

impl AsHandle for WaitPacket {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.0.as_handle()
    }
}

// Represents a UNICODE_STRING that owns its buffer, where the buffer is
// allocated on the Windows heap.
#[repr(transparent)]
pub struct UnicodeString(UNICODE_STRING);

// SAFETY: UnicodeString owns its heap-allocated pointers, which can be safely
//         aliased and sent between threads.
unsafe impl Send for UnicodeString {}
unsafe impl Sync for UnicodeString {}

#[derive(Debug)]
pub struct StringTooLong;

impl UnicodeString {
    pub fn new(s: &[u16]) -> std::result::Result<Self, StringTooLong> {
        let byte_count: u16 = (s.len() * 2).try_into().map_err(|_| StringTooLong)?;
        // FUTURE: use RtlProcessHeap instead of GetProcessHeap. This relies on
        // unstable Rust features to get the PEB.
        unsafe {
            let buf = RtlAllocateHeap(GetProcessHeap(), 0, byte_count.into()).cast::<u16>();
            assert!(!buf.is_null(), "out of memory");
            std::ptr::copy(s.as_ptr(), buf, s.len());
            Ok(Self(UNICODE_STRING {
                Length: byte_count,
                MaximumLength: byte_count,
                Buffer: buf,
            }))
        }
    }

    pub fn empty() -> Self {
        Self(unsafe { zeroed() })
    }

    pub fn is_empty(&self) -> bool {
        self.0.Buffer.is_null()
    }

    /// The length of the string in bytes.
    pub fn length(&self) -> usize {
        self.0.Length as usize
    }

    pub fn as_ptr(&self) -> *const UNICODE_STRING {
        &self.0
    }

    pub fn as_mut_ptr(&mut self) -> *mut UNICODE_STRING {
        &mut self.0
    }

    pub fn into_raw(mut self) -> UNICODE_STRING {
        let raw = self.0;
        self.0.Length = 0;
        self.0.MaximumLength = 0;
        self.0.Buffer = null_mut();
        raw
    }

    pub fn as_slice(&self) -> &[u16] {
        let buffer = NonNull::new(self.0.Buffer).unwrap_or_else(NonNull::dangling);
        unsafe { std::slice::from_raw_parts(buffer.as_ptr(), self.0.Length as usize / 2) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u16] {
        let buffer = NonNull::new(self.0.Buffer).unwrap_or_else(NonNull::dangling);
        unsafe { std::slice::from_raw_parts_mut(buffer.as_ptr(), self.0.Length as usize / 2) }
    }
}

impl Drop for UnicodeString {
    fn drop(&mut self) {
        unsafe {
            RtlFreeUnicodeString(&mut self.0);
        }
    }
}

impl<'a> TryFrom<&'a OsStr> for UnicodeString {
    type Error = StringTooLong;
    fn try_from(value: &'a OsStr) -> std::result::Result<Self, Self::Error> {
        // FUTURE: figure out how to do this without a second allocation.
        let value16: Vec<_> = value.encode_wide().collect();
        Self::new(&value16)
    }
}

impl<'a> TryFrom<&'a str> for UnicodeString {
    type Error = StringTooLong;
    fn try_from(value: &'a str) -> std::result::Result<Self, Self::Error> {
        Self::try_from(OsStr::new(value))
    }
}

impl TryFrom<String> for UnicodeString {
    type Error = StringTooLong;
    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        Self::try_from(OsStr::new(&value))
    }
}

impl<'a> TryFrom<&'a Path> for UnicodeString {
    type Error = StringTooLong;
    fn try_from(value: &'a Path) -> std::result::Result<Self, Self::Error> {
        Self::try_from(OsStr::new(value))
    }
}

#[repr(transparent)]
#[derive(Copy, Clone)]
pub struct UnicodeStringRef<'a>(UNICODE_STRING, PhantomData<&'a [u16]>);

impl<'a> UnicodeStringRef<'a> {
    pub fn new(s: &'a [u16]) -> Option<Self> {
        let len: u16 = (s.len() * 2).try_into().ok()?;
        Some(Self(
            UNICODE_STRING {
                Length: len,
                MaximumLength: len,
                Buffer: s.as_ptr().cast_mut(),
            },
            PhantomData,
        ))
    }

    pub fn empty() -> Self {
        Self(unsafe { zeroed() }, PhantomData)
    }

    pub fn is_empty(&self) -> bool {
        self.0.Buffer.is_null()
    }

    pub fn as_ptr(&self) -> *const UNICODE_STRING {
        &self.0
    }

    pub fn as_mut_ptr(&mut self) -> *mut UNICODE_STRING {
        &mut self.0
    }

    pub fn as_slice(&self) -> &[u16] {
        let buffer = NonNull::new(self.0.Buffer).unwrap_or_else(NonNull::dangling);
        unsafe { std::slice::from_raw_parts(buffer.as_ptr(), self.0.Length as usize / 2) }
    }
}

pub trait AsUnicodeStringRef {
    fn as_unicode_string_ref(&self) -> &UnicodeStringRef<'_>;
}

impl<T: AsUnicodeStringRef> AsUnicodeStringRef for &T {
    fn as_unicode_string_ref(&self) -> &UnicodeStringRef<'_> {
        (*self).as_unicode_string_ref()
    }
}

impl AsUnicodeStringRef for UnicodeString {
    fn as_unicode_string_ref(&self) -> &UnicodeStringRef<'_> {
        // SAFETY: &UnicodeStringRef can be safely transmuted from
        // &UNICODE_STRING as long as the lifetimes are correct, and they are
        // here because the UnicodeStringRef will live no longer than self.
        unsafe { std::mem::transmute(&self.0) }
    }
}

impl AsUnicodeStringRef for UnicodeStringRef<'_> {
    fn as_unicode_string_ref(&self) -> &UnicodeStringRef<'_> {
        self
    }
}

impl AsRef<windows::Win32::Foundation::UNICODE_STRING> for UnicodeStringRef<'_> {
    fn as_ref(&self) -> &windows::Win32::Foundation::UNICODE_STRING {
        // SAFETY: These are different definitions of the same type, so the memory layout is the
        // same.
        unsafe { std::mem::transmute(&self.0) }
    }
}

impl<'a> TryFrom<&'a Utf16Str> for UnicodeStringRef<'a> {
    type Error = StringTooLong;

    fn try_from(value: &'a Utf16Str) -> std::result::Result<Self, Self::Error> {
        UnicodeStringRef::new(value.as_slice()).ok_or(StringTooLong)
    }
}

/// Associates an ANSI_STRING with the lifetime of the buffer.
#[repr(transparent)]
pub struct AnsiStringRef<'a>(ANSI_STRING, PhantomData<&'a [u8]>);

impl<'a> AnsiStringRef<'a> {
    /// Creates a new `AnsiStringRef` using the specified buffer.
    ///
    /// Returns `None` if the buffer is too big for an ANSI_STRING's maximum length.
    pub fn new(s: &'a [u8]) -> Option<Self> {
        let len: u16 = s.len().try_into().ok()?;
        Some(Self(
            ANSI_STRING {
                Length: len,
                MaximumLength: len,
                Buffer: s.as_ptr() as *mut i8,
            },
            PhantomData,
        ))
    }

    /// Creates an empty `AnsiStringRef` with no buffer.
    pub fn empty() -> Self {
        Self(unsafe { zeroed() }, PhantomData)
    }

    /// Gets a value which indicates whether this instance does not contain a buffer.
    pub fn is_empty(&self) -> bool {
        self.0.Buffer.is_null()
    }

    /// Returns a pointer to the contained `ANSI_STRING`
    pub fn as_ptr(&self) -> *const ANSI_STRING {
        &self.0
    }

    /// Returns a mutable pointer to the contained `ANSI_STRING`
    pub fn as_mut_ptr(&mut self) -> *mut ANSI_STRING {
        &mut self.0
    }

    /// Returns the valid part of an ANSI_STRING's buffer as a slice.
    pub fn as_slice(&self) -> &[u8] {
        let buffer = NonNull::new(self.0.Buffer.cast::<u8>()).unwrap_or_else(NonNull::dangling);
        unsafe { std::slice::from_raw_parts(buffer.as_ptr(), self.0.Length as usize) }
    }
}

impl AsRef<ANSI_STRING> for AnsiStringRef<'_> {
    fn as_ref(&self) -> &ANSI_STRING {
        &self.0
    }
}

pub fn status_to_error(status: i32) -> Error {
    Error::from_raw_os_error(unsafe { RtlNtStatusToDosErrorNoTeb(status) } as i32)
}

pub fn chk_status(status: i32) -> Result<i32> {
    if status >= 0 {
        Ok(status)
    } else {
        Err(status_to_error(status))
    }
}

pub fn dos_to_nt_path<P: AsRef<Path>>(path: P) -> Result<UnicodeString> {
    let path16 = U16CString::from_os_str(path.as_ref().as_os_str())
        .map_err(|_| Error::from_raw_os_error(ERROR_BAD_PATHNAME as i32))?;
    let mut pathu = UnicodeString::empty();
    unsafe {
        chk_status(RtlDosPathNameToNtPathName_U_WithStatus(
            path16.as_ptr().cast_mut(),
            pathu.as_mut_ptr(),
            null_mut(),
            null_mut(),
        ))?;
    }
    Ok(pathu)
}

/// A wrapper around OBJECT_ATTRIBUTES.
#[repr(transparent)]
pub struct ObjectAttributes<'a> {
    attributes: ntdef::OBJECT_ATTRIBUTES,
    phantom: PhantomData<&'a ()>,
}

impl Default for ObjectAttributes<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> ObjectAttributes<'a> {
    /// Constructs the default object attributes, with no name, root directory,
    /// attributes, or security information.
    pub fn new() -> Self {
        Self {
            attributes: ntdef::OBJECT_ATTRIBUTES {
                Length: size_of::<ntdef::OBJECT_ATTRIBUTES>() as u32,
                RootDirectory: null_mut(),
                ObjectName: null_mut(),
                Attributes: 0,
                SecurityDescriptor: null_mut(),
                SecurityQualityOfService: null_mut(),
            },
            phantom: PhantomData,
        }
    }

    /// Sets the object name to `name`.
    pub fn name<P>(&mut self, name: &'a P) -> &mut Self
    where
        P: AsUnicodeStringRef,
    {
        self.attributes.ObjectName = name.as_unicode_string_ref().as_ptr().cast_mut();
        self
    }

    /// Sets the root directory to `root`.
    pub fn root(&mut self, root: BorrowedHandle<'a>) -> &mut Self {
        self.attributes.RootDirectory = root.as_raw_handle();
        self
    }

    /// Sets the attributes to `attributes`.
    pub fn attributes(&mut self, attributes: u32) -> &mut Self {
        self.attributes.Attributes = attributes;
        self
    }

    /// Sets the security descriptor to `sd`.
    pub fn security_descriptor(&mut self, sd: &'a SecurityDescriptor) -> &mut Self {
        self.attributes.SecurityDescriptor = sd.as_ptr();
        self
    }

    /// Returns the OBJECT_ATTRIBUTES pointer for passing to an NT syscall.
    pub fn as_ptr(&self) -> *mut ntdef::OBJECT_ATTRIBUTES {
        std::ptr::from_ref(&self.attributes).cast_mut()
    }
}

impl AsRef<windows::Wdk::Foundation::OBJECT_ATTRIBUTES> for ObjectAttributes<'_> {
    fn as_ref(&self) -> &windows::Wdk::Foundation::OBJECT_ATTRIBUTES {
        // SAFETY: These are different definitions of the same type, so the memory layout is the
        // same.
        unsafe { std::mem::transmute(&self.attributes) }
    }
}

pub fn open_object_directory(obj_attr: &ObjectAttributes<'_>, access: u32) -> Result<OwnedHandle> {
    // SAFETY: calling the API according to the NT API
    unsafe {
        let mut handle = null_mut();
        chk_status(NtOpenDirectoryObject(
            &mut handle,
            access,
            obj_attr.as_ptr(),
        ))?;
        Ok(OwnedHandle::from_raw_handle(handle))
    }
}

pub fn create_object_directory(
    obj_attr: &ObjectAttributes<'_>,
    access: u32,
) -> Result<OwnedHandle> {
    // SAFETY: calling the API according to the NT API
    unsafe {
        let mut handle = null_mut();
        chk_status(NtCreateDirectoryObject(
            &mut handle,
            access,
            obj_attr.as_ptr(),
        ))?;
        Ok(OwnedHandle::from_raw_handle(handle))
    }
}

/// A wrapper around memory that was allocated with `RtlAllocateHeap` and will be freed on drop with `RtlFreeHeap`,
/// like [`std::boxed::Box`].
pub struct RtlHeapBox<T: ?Sized> {
    value: NonNull<T>,
}

impl<T> RtlHeapBox<T> {
    /// Creates a new `RtlHeapBox` from a raw pointer.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that the pointer was allocated with `RtlAllocateHeap` with the default heap of the current
    /// process as the heap handle, returned by `GetProcessHeap`.
    ///
    /// The caller must not allow this pointer to be aliased anywhere else. Conceptually, by calling `from_raw`, the caller
    /// must guarantee that ownership of the pointer `value` is transferred to this `RtlHeapBox`. This is to uphold the aliasing
    /// requirements used by `RtlHeapBox` to implement various Deref and AsRef traits.
    ///
    /// On drop, this memory will be freed with `RtlFreeHeap`. The caller must not manually free this pointer.
    pub unsafe fn from_raw(value: *mut T) -> Self {
        Self {
            value: NonNull::new(value).unwrap(),
        }
    }

    /// Gets the contained pointer.
    pub fn as_ptr(&self) -> *const T {
        self.value.as_ptr()
    }
}

impl<T> std::ops::Deref for RtlHeapBox<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        // SAFETY: The pointer held by the RtlHeapBox is guaranteed to be valid and conform to the rules required by NonNull::as_ref.
        unsafe { self.value.as_ref() }
    }
}

impl<T> std::ops::DerefMut for RtlHeapBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: The pointer held by the RtlHeapBox is guaranteed to be valid and conform to the rules required by NonNull::as_mut.
        unsafe { self.value.as_mut() }
    }
}

impl<T> AsRef<T> for RtlHeapBox<T> {
    fn as_ref(&self) -> &T {
        // SAFETY: The pointer held by the RtlHeapBox is guaranteed to be valid and conform to the rules required by NonNull::as_ref.
        unsafe { self.value.as_ref() }
    }
}

impl<T> AsMut<T> for RtlHeapBox<T> {
    fn as_mut(&mut self) -> &mut T {
        // SAFETY: The pointer held by the RtlHeapBox is guaranteed to be valid and conform to the rules required by NonNull::as_mut.
        unsafe { self.value.as_mut() }
    }
}

impl<T: ?Sized> Drop for RtlHeapBox<T> {
    fn drop(&mut self) {
        // SAFETY: The pointer held by the RtlHeapBox must be allocated via RtlAllocateHeap from the constraints in
        //         RtlHeapBox::from_raw.
        unsafe {
            ntrtl::RtlFreeHeap(GetProcessHeap(), 0, self.value.as_ptr().cast::<c_void>());
        }
    }
}

/// A wrapper around a sized buffer that was allocated with RtlAllocateHeap. This allows extracting a slice via helper methods
/// instead of using [`RtlHeapBox`] directly.
pub struct RtlHeapBuffer {
    buffer: RtlHeapBox<u8>,
    size: usize,
}

impl RtlHeapBuffer {
    /// Creates a new `HeapBuffer` from a raw pointer and size.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that the pointer `buffer` conforms to the safety requirements imposed by [`RtlHeapBox::from_raw`].
    ///
    /// Additionally, the pointer described by `buffer` must describe a [`u8`] array of count `size`.
    pub unsafe fn from_raw(buffer: *mut u8, size: usize) -> RtlHeapBuffer {
        Self {
            // SAFETY: The caller has guaranteed that this pointer is an RtlAllocateHeap pointer and should be managed
            //         via a RtlHeapBox.
            buffer: unsafe { RtlHeapBox::from_raw(buffer) },
            size,
        }
    }
}

impl std::ops::Deref for RtlHeapBuffer {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        // SAFETY: The pointer described by buffer is a u8 array of self.size, as required in RtlHeapBuffer::from_raw.
        unsafe { std::slice::from_raw_parts(self.buffer.as_ptr(), self.size) }
    }
}

/// `Send`+`Sync` wrapper around `OVERLAPPED`.
///
/// Internally uses an UnsafeCell since this may be concurrently updated by the
/// kernel.
#[repr(transparent)]
#[derive(Default, Debug)]
pub struct Overlapped(UnsafeCell<OVERLAPPED>);

impl Overlapped {
    pub fn new() -> Self {
        Default::default()
    }

    /// Sets the offset for the IO request.
    pub fn set_offset(&mut self, offset: i64) {
        let overlapped = self.0.get_mut();
        // SAFETY: Writing to union field.
        unsafe {
            overlapped.u.s_mut().Offset = offset as u32;
            overlapped.u.s_mut().OffsetHigh = (offset >> 32) as u32;
        }
    }

    pub fn set_event(&mut self, event: RawHandle) {
        self.0.get_mut().hEvent = event;
    }

    pub fn as_ptr(&self) -> *mut OVERLAPPED {
        self.0.get()
    }

    /// Polls the current operation status.
    pub fn io_status(&self) -> Option<(NTSTATUS, usize)> {
        let overlapped = self.0.get();
        // SAFETY: The kernel might be mutating the overlapped structure right
        // now, so this gets a &AtomicUsize just to the Internal field that
        // contains the completion status.
        let internal = unsafe { &*addr_of!((*overlapped).Internal).cast::<AtomicUsize>() };
        let status = internal.load(Ordering::Acquire) as NTSTATUS;
        if status != STATUS_PENDING {
            // SAFETY: the IO is complete so it's safe to read this value directly.
            let information = unsafe { (*self.0.get()).InternalHigh };
            Some((status, information))
        } else {
            None
        }
    }
}

// SAFETY: By itself, an overlapped structure can be safely sent or shared
// across multiple threads. Of course, while it is owned by the kernel is cannot
// be concurrently accessed, but this has no bearing on its Send/Sync-ness.
unsafe impl Send for Overlapped {}
// SAFETY: See above comment.
unsafe impl Sync for Overlapped {}

#[macro_export]
macro_rules! delayload {
    {$dll:literal {
        $(
            $(#[$a:meta])*
            $visibility:vis fn $name:ident($($params:ident : $types:ty),* $(,)?) -> $result:ty;
        )*
    }} => {
        fn get_module() -> Result<::windows_sys::Win32::Foundation::HMODULE, ::windows_sys::Win32::Foundation::WIN32_ERROR> {
            use ::std::ffi::c_void;
            use ::std::ptr::null_mut;
            use ::std::sync::atomic::{AtomicPtr, Ordering};
            use ::windows_sys::Win32::{
                Foundation::{FreeLibrary, GetLastError},
                System::LibraryLoader::{LoadLibraryA},
            };

            static MODULE: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
            let mut module = MODULE.load(Ordering::Relaxed);
            if module.is_null() {
                module = unsafe { LoadLibraryA(concat!($dll, "\0").as_ptr().cast::<u8>()) };
                if module.is_null() {
                    return Err(unsafe { GetLastError() });
                }
                let old_module = MODULE.swap(module, Ordering::Relaxed);
                if !old_module.is_null() {
                    unsafe { FreeLibrary(old_module) };
                }
            }
            Ok(module)
        }

        mod funcs {
            #![expect(non_snake_case)]
            $(
                $(#[$a])*
                pub fn $name() -> Result<usize, ::windows_sys::Win32::Foundation::WIN32_ERROR> {
                    use ::std::concat;
                    use ::std::sync::atomic::{AtomicUsize, Ordering};
                    use ::windows_sys::Win32::{
                        Foundation::ERROR_PROC_NOT_FOUND,
                        System::LibraryLoader::GetProcAddress,
                    };

                    // A FNCELL value 0 denotes that GetProcAddress has never been
                    // called for the given function.
                    // A FNCELL value 1 denotes that GetProcAddress has been called
                    // but the procedure does not exist.
                    // Any other FNCELL value denotes the result of GetProcAddress,
                    // the callable adress of the function.
                    static FNCELL: AtomicUsize = AtomicUsize::new(0);
                    let mut fnval = FNCELL.load(Ordering::Relaxed);
                    if fnval == 0 {
                        let module = super::get_module()?;
                        fnval = unsafe { GetProcAddress(
                            module,
                            concat!(stringify!($name), "\0").as_ptr().cast::<u8>()) }
                            .map(|f| f as usize)
                            .unwrap_or(0);
                        if fnval == 0 {
                            fnval = 1;
                        }
                        FNCELL.store(fnval, Ordering::Relaxed);
                    }
                    if fnval == 1 {
                        return Err(ERROR_PROC_NOT_FOUND)
                    }
                    Ok(fnval)
                }
            )*
        }

        pub mod is_supported {
            #![expect(non_snake_case)]
            #![allow(dead_code)]
            $(
                $(#[$a])*
                pub fn $name() -> bool {
                    super::funcs::$name().is_ok()
                }
            )*
        }

        $(
            $(#[$a])*
            #[expect(non_snake_case)]
            $visibility unsafe fn $name($($params: $types,)*) -> $result {
                match funcs::$name() {
                    Ok(fnval) => {
                        type FnType = unsafe extern "system" fn($($params: $types,)*) -> $result;
                        // SAFETY: fnval is a valid function pointer obtained from GetProcAddress
                        let fnptr: FnType = unsafe { ::std::mem::transmute(fnval) };
                        // SAFETY: The function pointer is valid and the caller must uphold the function's safety contract
                        unsafe { fnptr($($params,)*) }
                    },
                    Err(win32) => {
                        $crate::delayload!(@result_from_win32(($result), win32))
                    }
                }
            }
        )*
    };

    (@result_from_win32((i32), $val:expr)) => { ::windows_result::HRESULT::from_win32($val) };
    (@result_from_win32((u32), $val:expr)) => { $val };
    (@result_from_win32((DWORD), $val:expr)) => { $val };
    (@result_from_win32((HRESULT), $val:expr)) => { ::windows_result::HRESULT::from_win32($val) };
    (@result_from_win32(($t:tt), $val:expr)) => { panic!("could not load: {}", $val) };
}

/// Closes stdout, replacing it with the null device.
pub fn close_stdout() -> Result<()> {
    let new_stdout = File::open("nul")?;
    let stdout = io::stdout();
    // Prevent concurrent accesses to stdout.
    let _locked = stdout.lock();
    let old_handle = stdout.as_raw_handle();
    // SAFETY: transferring ownership of the new handle.
    unsafe {
        if SetStdHandle(STD_OUTPUT_HANDLE, new_stdout.into_raw_handle()) == 0 {
            panic!("failed to set handle");
        }
    }
    drop(_locked);
    unsafe {
        // SAFETY: the old handle is no longer referenced anywhere.
        CloseHandle(old_handle);
    }

    Ok(())
}

/// Disables the hard error dialog on "critical errors".
pub fn disable_hard_error_dialog() {
    // SAFETY: This Win32 API has no safety requirements.
    unsafe {
        SetErrorMode(GetErrorMode() | SEM_FAILCRITICALERRORS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dos_to_nt_path() {
        let pathu = dos_to_nt_path("c:\\foo").unwrap();
        assert!(
            pathu
                .as_slice()
                .iter()
                .copied()
                .eq("\\??\\c:\\foo".encode_utf16())
        );
    }

    #[test]
    fn test_alloc_unicode_string() {
        let s: UnicodeString = "abc".try_into().unwrap();
        assert!(s.as_slice().iter().copied().eq("abc".encode_utf16()));
    }

    #[test]
    fn test_delayload() {
        // Test delayloading kernel32.dll functions that are guaranteed to exist
        mod kernel32_delayload {
            delayload! { "kernel32.dll" {
                pub fn GetCurrentProcessId() -> u32;
                pub fn GetCurrentThreadId() -> u32;
            }}
        }

        // Test is_supported for functions that exist
        assert!(kernel32_delayload::is_supported::GetCurrentProcessId());
        assert!(kernel32_delayload::is_supported::GetCurrentThreadId());

        // Test that the functions work correctly
        unsafe {
            let pid = kernel32_delayload::GetCurrentProcessId();
            assert_ne!(pid, 0, "Process ID should not be zero");

            let tid = kernel32_delayload::GetCurrentThreadId();
            assert_ne!(tid, 0, "Thread ID should not be zero");
        }
    }

    #[test]
    fn test_delayload_missing_function() {
        // Test delayloading a function that doesn't exist
        mod kernel32_missing {
            delayload! { "kernel32.dll" {
                #[allow(dead_code)]
                pub fn NonExistentFunction() -> u32;
            }}
        }

        // Test that is_supported correctly identifies missing functions
        assert!(!kernel32_missing::is_supported::NonExistentFunction());
    }

    #[test]
    fn test_delayload_missing_dll() {
        // Test delayloading from a DLL that doesn't exist
        mod missing_dll {
            delayload! { "this_dll_does_not_exist_12345.dll" {
                #[allow(dead_code)]
                pub fn SomeFunction() -> u32;
            }}
        }

        // Test that is_supported returns false when the DLL doesn't exist
        assert!(!missing_dll::is_supported::SomeFunction());
    }

    #[test]
    fn test_delayload_multiple_calls() {
        // Test that multiple calls to the same delayloaded function work
        // and that the function pointer is cached correctly
        mod kernel32_cached {
            delayload! { "kernel32.dll" {
                pub fn GetCurrentProcessId() -> u32;
            }}
        }

        unsafe {
            let pid1 = kernel32_cached::GetCurrentProcessId();
            let pid2 = kernel32_cached::GetCurrentProcessId();
            let pid3 = kernel32_cached::GetCurrentProcessId();

            // All calls should return the same PID
            assert_eq!(pid1, pid2);
            assert_eq!(pid2, pid3);
        }
    }
}
