// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows overlapped IO support.

use crate::driver::Driver;
use crate::driver::PollImpl;
use crate::waker::WakerList;
use pal::windows::chk_status;
use pal::windows::Overlapped;
use pal::windows::SendSyncRawHandle;
use parking_lot::Mutex;
use std::fs::File;
use std::future::Future;
use std::io;
use std::mem::ManuallyDrop;
use std::os::windows::prelude::*;
use std::pin::Pin;
use std::ptr::null;
use std::ptr::null_mut;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use winapi::um::fileapi::ReadFile;
use winapi::um::fileapi::WriteFile;
use winapi::um::ioapiset::CancelIoEx;
use winapi::um::ioapiset::DeviceIoControl;
use winapi::um::minwinbase::OVERLAPPED;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Driver methods for supporting overlapped files.
pub trait OverlappedIoDriver: Unpin {
    /// The handler type.
    type OverlappedIo: 'static + IoOverlapped;

    /// Creates a new overlapped file handler.
    ///
    /// # Safety
    ///
    /// The caller must ensure that they exclusively own `handle`, and that
    /// `handle` stays alive until the new handler is dropped.
    unsafe fn new_overlapped_file(&self, handle: RawHandle) -> io::Result<Self::OverlappedIo>;
}

/// Methods for handling overlapped IO.
pub trait IoOverlapped: Unpin + Send + Sync {
    /// Prepares for an overlapped IO.
    fn pre_io(&self);

    /// Notifies that an IO has been issued.
    ///
    /// Returns true if the IO has completed synchronously, false if
    /// `overlapped_io_complete` will later be called to indicate completion.
    ///
    /// # Safety
    /// The caller must have called `pre_io`, and `overlapped` must be
    /// associated with an IO whose syscall just returned `result`. If this
    /// routine returned `false`, the caller must not deallocate `overlapped`
    /// until `overlapped_io_complete` is called.
    unsafe fn post_io(&self, result: &io::Result<()>, overlapped: &Overlapped) -> bool;
}

/// A file opened for overlapped IO.
pub struct OverlappedFile {
    inner: PollImpl<dyn IoOverlapped>,
    file: File,
}

impl OverlappedFile {
    /// Prepares `file` for overlapped IO.
    ///
    /// `file` must have been opeend with `FILE_FLAG_OVERLAPPED`.
    pub fn new(driver: &(impl ?Sized + Driver), file: File) -> io::Result<Self> {
        let inner = unsafe { driver.new_dyn_overlapped_file(file.as_raw_handle())? };
        Ok(Self { inner, file })
    }

    /// Returns the inner file.
    pub fn into_inner(self) -> File {
        drop(self.inner);
        self.file
    }

    /// Gets the inner file.
    pub fn get(&self) -> &File {
        &self.file
    }

    /// Cancels all IO for this file.
    pub fn cancel(&self) {
        // SAFETY: File handle is owned by self.
        unsafe {
            CancelIoEx(self.file.as_raw_handle(), null_mut());
        }
    }
}

#[derive(Debug)]
struct Io<T> {
    inner: ManuallyDrop<Box<IoInner<T>>>,
    handle: Option<SendSyncRawHandle>,
}

impl<T> Default for Io<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(C)]
#[derive(Debug)]
struct IoInner<T> {
    overlapped: Overlapped,
    state: Mutex<IoState>,
    buffers: Option<T>,
}

#[derive(Debug)]
enum IoState {
    None,
    Issued,
    SyncError(Option<io::Error>),
    Waiting(Waker),
    Dropped(unsafe fn(*mut ())),
}

impl<T> Io<T> {
    fn new() -> Self {
        let inner = Box::new(IoInner {
            overlapped: Overlapped::new(),
            state: Mutex::new(IoState::None),
            buffers: None,
        });
        Self {
            inner: ManuallyDrop::new(inner),
            handle: None,
        }
    }

    fn issue<F>(&mut self, file: &OverlappedFile, offset: i64, buffers: T, f: F)
    where
        F: FnOnce(RawHandle, &mut T, *mut OVERLAPPED) -> io::Result<()>,
    {
        assert!(self.handle.is_none());
        self.inner.overlapped.set_offset(offset);
        *self.inner.state.get_mut() = IoState::Issued;
        let overlapped = self.inner.overlapped.as_ptr();
        let buffers = self.inner.buffers.insert(buffers);
        let handle = file.file.as_raw_handle();

        file.inner.pre_io();
        let result = f(handle, buffers, overlapped);
        if unsafe { file.inner.post_io(&result, &self.inner.overlapped) } {
            // The IO completed synchronously. If an error was returned, store it because the IO
            // status block is not updated in this case.
            *self.inner.state.get_mut() = result
                .map(|_| IoState::None)
                .unwrap_or_else(|e| IoState::SyncError(Some(e)));
        } else {
            self.handle = Some(SendSyncRawHandle(handle));
        }
    }

    fn cancel(&mut self) {
        if let Some(handle) = &self.handle {
            unsafe {
                CancelIoEx(handle.0, self.inner.overlapped.as_ptr());
            }
        }
    }

    fn poll_result(&mut self, cx: &mut Context<'_>) -> Poll<BufResult<T>> {
        if let Some(result) = self.try_complete() {
            Poll::Ready(result)
        } else {
            let old_state = std::mem::replace(
                &mut *self.inner.state.lock(),
                IoState::Waiting(cx.waker().clone()),
            );
            match old_state {
                IoState::None => Poll::Ready(self.try_complete().unwrap()),
                IoState::Issued | IoState::Waiting(_) => Poll::Pending,
                IoState::Dropped(_) | IoState::SyncError(_) => unreachable!(),
            }
        }
    }

    fn try_complete(&mut self) -> Option<BufResult<T>> {
        // If an error was returned synchronously the IO status block is not updated, so check for
        // that before accessing the overlapped structure.
        let result = if let IoState::SyncError(error) = self.inner.state.get_mut() {
            Err(error.take().unwrap())
        } else {
            let (status, len) = self.inner.overlapped.io_status()?;
            chk_status(status).map(|_| len)
        };

        let buffers = self.inner.buffers.take().unwrap();
        Some((result, buffers))
    }
}

impl<T: IoBufMut> Io<T> {
    fn read(&mut self, file: &OverlappedFile, offset: i64, buffers: T) {
        self.issue(
            file,
            offset,
            buffers,
            |handle, buffers, overlapped| unsafe {
                if ReadFile(
                    handle,
                    buffers.as_mut_ptr().cast(),
                    buffers.len().min(u32::MAX as usize) as u32,
                    null_mut(),
                    overlapped,
                ) != 0
                {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            },
        );
    }
}

impl<T: IoBuf> Io<T> {
    fn write(&mut self, file: &OverlappedFile, offset: i64, buffers: T) {
        self.issue(
            file,
            offset,
            buffers,
            |handle, buffers, overlapped| unsafe {
                if WriteFile(
                    handle,
                    buffers.as_ptr().cast(),
                    buffers.len().min(u32::MAX as usize) as u32,
                    null_mut(),
                    overlapped,
                ) != 0
                {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            },
        );
    }
}

impl<T: IoBufMut, U: IoBufMut> Io<(T, U)> {
    fn ioctl(&mut self, file: &OverlappedFile, code: u32, input: T, output: U) {
        self.issue(
            file,
            0,
            (input, output),
            |handle, (input, output), overlapped| unsafe {
                if DeviceIoControl(
                    handle,
                    code,
                    input.as_mut_ptr().cast(),
                    input.len() as u32,
                    output.as_mut_ptr().cast(),
                    output.len() as u32,
                    null_mut(),
                    overlapped,
                ) != 0
                {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            },
        );
    }
}

pub(crate) unsafe fn overlapped_io_done(overlapped: *mut OVERLAPPED, wakers: &mut WakerList) {
    let inner = overlapped as *const IoInner<()>;
    let old_state = std::mem::replace(&mut *unsafe { &(*inner).state }.lock(), IoState::None);
    match old_state {
        IoState::None | IoState::SyncError(_) => unreachable!(),
        IoState::Issued => {}
        IoState::Waiting(waker) => wakers.push(waker),
        IoState::Dropped(drop_fn) => unsafe { drop_fn(inner as *mut ()) },
    }
}

impl<T> Drop for Io<T> {
    fn drop(&mut self) {
        if self.handle.is_some() {
            let drop_fn = |p: *mut ()| drop(unsafe { Box::from_raw(p.cast::<IoInner<T>>()) });
            let old_state =
                std::mem::replace(&mut *self.inner.state.lock(), IoState::Dropped(drop_fn));
            match old_state {
                IoState::None | IoState::SyncError(_) => {
                    // SAFETY: inner is no longer referenced by the kernel.
                    unsafe { ManuallyDrop::drop(&mut self.inner) };
                }
                IoState::Waiting(_) | IoState::Issued => {
                    // Ensure the IO completes soon so that buffers can be freed.
                    self.cancel();
                }
                IoState::Dropped(_) => unreachable!(),
            }
        }
    }
}

/// A non-movable buffer that owns its storage.
///
/// # Safety
/// The implementor must ensure that the methods are implemented as described.
pub unsafe trait IoBuf {
    /// Returns a stable pointer to the storage.
    fn as_ptr(&self) -> *const u8;
    /// Returns the length of the storage in bytes.
    fn len(&self) -> usize;
}

/// A mutable non-movable buffer that owns its storage.
///
/// # Safety
/// The implementor must ensure that the methods are implemented as described.
pub unsafe trait IoBufMut: IoBuf {
    /// Returns a stable mutable pointer to the storage.
    fn as_mut_ptr(&mut self) -> *mut u8;
}

unsafe impl<T> IoBuf for [T; 0] {
    fn as_ptr(&self) -> *const u8 {
        null()
    }

    fn len(&self) -> usize {
        0
    }
}

unsafe impl<T> IoBufMut for [T; 0] {
    fn as_mut_ptr(&mut self) -> *mut u8 {
        null_mut()
    }
}

unsafe impl IoBuf for () {
    fn as_ptr(&self) -> *const u8 {
        null()
    }

    fn len(&self) -> usize {
        0
    }
}

unsafe impl IoBufMut for () {
    fn as_mut_ptr(&mut self) -> *mut u8 {
        null_mut()
    }
}

unsafe impl<T: IntoBytes + Immutable + KnownLayout> IoBuf for &'static [T] {
    fn as_ptr(&self) -> *const u8 {
        self.as_bytes().as_ptr()
    }

    fn len(&self) -> usize {
        self.as_bytes().len()
    }
}

unsafe impl<T: IntoBytes + Immutable + KnownLayout> IoBuf for Vec<T> {
    fn as_ptr(&self) -> *const u8 {
        self.as_bytes().as_ptr()
    }

    fn len(&self) -> usize {
        self.as_bytes().len()
    }
}

unsafe impl<T: IntoBytes + FromBytes + Immutable + KnownLayout> IoBufMut for Vec<T> {
    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.as_mut_bytes().as_mut_ptr()
    }
}

impl OverlappedFile {
    /// Reads from the file at `offset` into `buffer`.
    pub fn read_at<T: IoBufMut>(&self, offset: u64, buffer: T) -> Read<T> {
        let mut io = Io::new();
        io.read(self, offset as i64, buffer);
        Read(io)
    }

    /// Writes to the file at `offset` from `buffer`.
    pub fn write_at<T: IoBuf>(&self, offset: u64, buffer: T) -> Write<T> {
        let mut io = Io::new();
        io.write(self, offset as i64, buffer);
        Write(io)
    }

    /// Issues an IOCTL to the file.
    ///
    /// # Safety
    /// The caller must ensure the IOCTL is safe to call.
    pub unsafe fn ioctl<T: IoBufMut, U: IoBufMut>(
        &self,
        code: u32,
        input: T,
        output: U,
    ) -> Ioctl<T, U> {
        let mut io = Io::new();
        io.ioctl(self, code, input, output);
        Ioctl(io)
    }

    /// Performs a custom overlapped IO by calling `f`.
    ///
    /// # Safety
    /// The caller must issue the IO in `f` and return its syscall result. The
    /// kernel must only alias memory that is in `buffers`, and only after it
    /// has been moved into its final location (provided to `f` in the second
    /// parameter).
    pub unsafe fn custom<F, T>(&self, buffers: T, f: F) -> Custom<T>
    where
        F: FnOnce(RawHandle, &mut T, *mut OVERLAPPED) -> io::Result<()>,
    {
        let mut io = Io::new();
        io.issue(self, 0, buffers, f);
        Custom(io)
    }
}

/// An IO result that returns the associated buffers.
pub type BufResult<T> = (io::Result<usize>, T);

macro_rules! io {
    ($name:ident, ($($generics:ident),*), $buffers:ty) => {
        /// An IO operation.
        #[derive(Debug)]
        #[must_use]
        pub struct $name<$($generics,)*>(Io<$buffers>);

        impl<$($generics,)*> $name<$($generics,)*> {
            /// Requests that the kernel cancel the IO.
            ///
            /// This does not synchronously cancel the IO. Await the object to
            /// wait for the IO to complete.
            pub fn cancel(&mut self) {
                self.0.cancel()
            }

            /// Gets the completion result of the IO, returning `None` if the IO
            /// is still in flight.
            pub fn try_complete(&mut self) -> Option<BufResult<$buffers>> {
                self.0.try_complete()
            }
        }

        impl<$($generics,)*> Future for $name<$($generics,)*> {
            type Output = BufResult<$buffers>;

            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                self.get_mut().0.poll_result(cx)
            }
        }
    };
}

io!(Read, (T), T);
io!(Write, (T), T);
io!(Ioctl, (T, U), (T, U));
io!(Custom, (T), T);
