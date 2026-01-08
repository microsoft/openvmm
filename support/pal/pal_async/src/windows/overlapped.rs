// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows overlapped IO support.

use crate::driver::Driver;
use crate::driver::PollImpl;
use crate::waker::WakerList;
use pal::windows::Overlapped;
use pal::windows::SendSyncRawHandle;
use pal::windows::chk_status;
use parking_lot::Mutex;
use std::cell::UnsafeCell;
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
use windows_sys::Win32::Foundation::ERROR_IO_PENDING;
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
    /// `handle` stays alive until the new handler is dropped. The file must
    /// not be reused to issue IO without first disassociating it from this
    /// handler by calling [`IoOverlapped::disassociate`].
    unsafe fn new_overlapped_file(&self, handle: RawHandle) -> io::Result<Self::OverlappedIo>;
}

/// Methods for handling overlapped IO.
pub trait IoOverlapped: Unpin + Send + Sync {
    /// Prepares for an overlapped IO.
    fn pre_io(&self);

    /// Notifies that an IO has been issued.
    ///
    /// # Safety
    /// The caller must have called `pre_io`, and `overlapped` must be
    /// associated with an IO that either completed synchronously (`completed`
    /// is `true`) or is pending completion (`completed` is `false`).
    ///
    /// If `completed` is false, the caller must not deallocate `overlapped`
    /// until `overlapped_io_complete` is called for this IO.
    unsafe fn post_io(&self, completed: bool, overlapped: &Overlapped);

    /// Disassociates the file from the overlapped IO handler so that the file
    /// can be reused for other purposes.
    ///
    /// # Panic
    /// This function may panic if there are still pending IO operations
    /// associated with this handler.
    ///
    /// # Safety
    /// The caller must not call `pre_io` or `post_io` after calling this
    /// function.
    unsafe fn disassociate(&mut self);
}

/// A file opened for overlapped IO.
pub struct OverlappedFile {
    inner: PollImpl<dyn IoOverlapped>,
    file: File,
}

impl OverlappedFile {
    /// Prepares `file` for overlapped IO.
    ///
    /// `file` must have been opened with `FILE_FLAG_OVERLAPPED`.
    ///
    /// # Safety
    /// The caller must ensure that they exclusively own the underlying file,
    /// i.e., that the underlying handle has not been duplicated, and that it
    /// won't be used for asynchronous IO outside of this `OverlappedFile` until
    /// [`into_inner`](Self::into_inner) is called.
    pub unsafe fn new(driver: &(impl ?Sized + Driver), file: File) -> io::Result<Self> {
        // SAFETY: `file` is exclusively owned by the caller.
        let inner = unsafe { driver.new_dyn_overlapped_file(file.as_raw_handle())? };
        Ok(Self { inner, file })
    }

    /// Returns the inner file.
    ///
    /// # Panic
    /// This function will panic if the inner file is still in use by pending IO
    /// operations.
    pub fn into_inner(mut self) -> File {
        // SAFETY: `inner` is being dropped here, so no further IO will be issued.
        unsafe { self.inner.disassociate() };
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
    state: IssueState<T>,
}

#[derive(Debug)]
enum IssueState<T> {
    Pending {
        inner: ManuallyDrop<Pin<Box<IoInner<T>>>>,
        handle: SendSyncRawHandle,
    },
    Complete {
        inner: Box<IoInner<T>>,
        result: Result<(), io::Error>,
    },
    Taken,
}

#[repr(C)]
#[derive(Debug)]
struct IoInner<T> {
    overlapped: Overlapped,
    state: Mutex<InnerState>,
    // `buffers` is aliased and potentially mutated by the kernel while the IO
    // is pending, so use `UnsafeCell` to allow interior mutability.
    buffers: UnsafeCell<T>,
    // This structure cannot move while an IO is pending, since the kernel will
    // mutate its contents and keeps a pointer to its location. Prevent it from
    // being accidentally unpinned and moved.
    _pin: std::marker::PhantomPinned,
}

#[derive(Debug)]
enum InnerState {
    None,
    Issued,
    Waiting(Waker),
    Dropped(unsafe fn(*mut ())),
}

impl<T> Io<T> {
    fn issue<F>(file: &OverlappedFile, offset: i64, buffers: T, f: F) -> Self
    where
        F: FnOnce(RawHandle, &mut T, *mut OVERLAPPED) -> io::Result<()>,
    {
        let mut inner = Box::new(IoInner {
            overlapped: Overlapped::new(),
            state: Mutex::new(InnerState::Issued),
            buffers: UnsafeCell::new(buffers),
            _pin: std::marker::PhantomPinned,
        });
        inner.overlapped.set_offset(offset);

        let handle = file.file.as_raw_handle();
        file.inner.pre_io();
        let result = f(handle, inner.buffers.get_mut(), inner.overlapped.as_ptr());
        let completed = result.as_ref().map_or_else(
            |err| err.raw_os_error() != Some(ERROR_IO_PENDING as i32),
            |_| true,
        );
        // SAFETY: `pre_io` has been called with `overlapped` as the target.
        unsafe {
            file.inner.post_io(completed, &inner.overlapped);
        }
        let state = if completed {
            // The IO completed synchronously. If an error was returned, store
            // it because the IO status block is not updated in this case.
            IssueState::Complete { inner, result }
        } else {
            // Pin `inner` and avoid dropping it while it's still possibly in
            // use by the kernel.
            let inner = ManuallyDrop::new(Box::into_pin(inner));
            IssueState::Pending {
                inner,
                handle: SendSyncRawHandle(handle),
            }
        };
        Self { state }
    }

    fn cancel(&mut self) {
        if let IssueState::Pending { handle, inner } = &self.state {
            // SAFETY: the file handle is alive and the overlapped pointer is still valid.
            unsafe {
                CancelIoEx(handle.0, inner.overlapped.as_ptr());
            }
        }
    }

    fn poll_result(&mut self, cx: &mut Context<'_>) -> Poll<BufResult<T>> {
        let (r, inner) = match &self.state {
            IssueState::Complete { .. } => {
                let IssueState::Complete { result, inner } =
                    std::mem::replace(&mut self.state, IssueState::Taken)
                else {
                    unreachable!()
                };
                (result, inner)
            }
            IssueState::Pending { inner, .. } => {
                let mut state = inner.state.lock();
                match &mut *state {
                    InnerState::None => {
                        drop(state);
                        let IssueState::Pending { inner, .. } =
                            std::mem::replace(&mut self.state, IssueState::Taken)
                        else {
                            unreachable!()
                        };
                        // SAFETY: Since the IO is completed, `inner` is now
                        // exclusively owned so can be unpinned.
                        let inner =
                            unsafe { Pin::into_inner_unchecked(ManuallyDrop::into_inner(inner)) };
                        (Ok(()), inner)
                    }
                    InnerState::Issued => {
                        *state = InnerState::Waiting(cx.waker().clone());
                        return Poll::Pending;
                    }
                    InnerState::Waiting(waker) => {
                        waker.clone_from(cx.waker());
                        return Poll::Pending;
                    }
                    InnerState::Dropped(_) => unreachable!(),
                }
            }
            IssueState::Taken => panic!("polled after completion"),
        };

        let r = r.and_then(|()| {
            let (status, len) = inner
                .overlapped
                .io_status()
                .expect("IO is known to be complete");
            chk_status(status).map(|_| len)
        });

        let buffers = inner.buffers.into_inner();
        Poll::Ready((r, buffers))
    }
}

impl<T: IoBufMut> Io<T> {
    fn read(file: &OverlappedFile, offset: i64, buffers: T) -> Self {
        Self::issue(file, offset, buffers, |handle, buffers, overlapped| {
            // SAFETY: calling ReadFile with valid parameters.
            unsafe {
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
            }
        })
    }
}

impl<T: IoBuf> Io<T> {
    fn write(file: &OverlappedFile, offset: i64, buffers: T) -> Self {
        Self::issue(file, offset, buffers, |handle, buffers, overlapped| {
            // SAFETY: calling WriteFile with valid parameters.
            unsafe {
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
            }
        })
    }
}

impl<T: IoBufMut, U: IoBufMut> Io<(T, U)> {
    /// # Safety
    /// The caller must ensure the IOCTL is safe to call.
    unsafe fn ioctl(file: &OverlappedFile, code: u32, input: T, output: U) -> Self {
        Self::issue(
            file,
            0,
            (input, output),
            |handle, (input, output), overlapped| {
                // SAFETY: calling DeviceIoControl with valid parameters, according to the caller
                unsafe {
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
                }
            },
        )
    }
}

/// Called when an overlapped IO has completed.
///
/// # Safety
/// The caller must ensure that `overlapped` is a valid pointer to an overlapped
/// structure associated with an IO that has just completed.
pub(crate) unsafe fn overlapped_io_done(overlapped: *mut OVERLAPPED, wakers: &mut WakerList) {
    let inner = overlapped as *const IoInner<()>;
    let old_state = {
        // SAFETY: `inner` is currently shared between this function and the
        // owner of the `Io`.
        let inner = unsafe { &*inner };
        std::mem::replace(&mut *inner.state.lock(), InnerState::None)
    };
    match old_state {
        InnerState::None => unreachable!(),
        InnerState::Issued => {}
        InnerState::Waiting(waker) => wakers.push(waker),
        InnerState::Dropped(drop_fn) => {
            // SAFETY: `inner` is owned (since the original `Io` has been
            // dropped) and `drop_fn` is the correct function to drop it.
            unsafe { drop_fn(inner.cast_mut().cast()) }
        }
    }
}

impl<T> Drop for Io<T> {
    fn drop(&mut self) {
        match &mut self.state {
            IssueState::Taken | IssueState::Complete { .. } => {}
            IssueState::Pending { inner, .. } => {
                // An IO may still be pending.
                let old_state = std::mem::replace(
                    &mut *inner.state.lock(),
                    InnerState::Dropped(|p| {
                        // SAFETY: `p` is owned and is of the correct type.
                        unsafe { drop(Box::from_raw(p.cast::<IoInner<T>>())) };
                    }),
                );
                match old_state {
                    InnerState::None => {
                        // SAFETY: inner is now exclusively owned.
                        unsafe { ManuallyDrop::drop(inner) };
                    }
                    InnerState::Waiting(_) | InnerState::Issued => {
                        // Ensure the IO completes soon so that buffers can be freed.
                        self.cancel();
                    }
                    InnerState::Dropped(_) => unreachable!(),
                }
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

// SAFETY: implementing trait according to requirements.
unsafe impl<T> IoBuf for [T; 0] {
    fn as_ptr(&self) -> *const u8 {
        null()
    }

    fn len(&self) -> usize {
        0
    }
}

// SAFETY: implementing trait according to requirements.
unsafe impl<T> IoBufMut for [T; 0] {
    fn as_mut_ptr(&mut self) -> *mut u8 {
        null_mut()
    }
}

// SAFETY: implementing trait according to requirements.
unsafe impl IoBuf for () {
    fn as_ptr(&self) -> *const u8 {
        null()
    }

    fn len(&self) -> usize {
        0
    }
}

// SAFETY: implementing trait according to requirements.
unsafe impl IoBufMut for () {
    fn as_mut_ptr(&mut self) -> *mut u8 {
        null_mut()
    }
}

// SAFETY: implementing trait according to requirements.
unsafe impl<T: IntoBytes + Immutable + KnownLayout> IoBuf for &'static [T] {
    fn as_ptr(&self) -> *const u8 {
        self.as_bytes().as_ptr()
    }

    fn len(&self) -> usize {
        self.as_bytes().len()
    }
}

// SAFETY: implementing trait according to requirements.
unsafe impl<T: IntoBytes + Immutable + KnownLayout> IoBuf for Vec<T> {
    fn as_ptr(&self) -> *const u8 {
        self.as_bytes().as_ptr()
    }

    fn len(&self) -> usize {
        self.as_bytes().len()
    }
}

// SAFETY: implementing trait according to requirements.
unsafe impl<T: IntoBytes + FromBytes + Immutable + KnownLayout> IoBufMut for Vec<T> {
    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.as_mut_bytes().as_mut_ptr()
    }
}

impl OverlappedFile {
    /// Reads from the file at `offset` into `buffer`.
    pub fn read_at<T: IoBufMut>(&self, offset: u64, buffer: T) -> Read<T> {
        Read(Io::read(self, offset as i64, buffer))
    }

    /// Writes to the file at `offset` from `buffer`.
    pub fn write_at<T: IoBuf>(&self, offset: u64, buffer: T) -> Write<T> {
        Write(Io::write(self, offset as i64, buffer))
    }

    /// Issues an IOCTL to the file.
    ///
    /// # Safety
    /// The caller must ensure the IOCTL is safe to call. This is device and
    /// IOCTL specific.
    pub unsafe fn ioctl<T: IoBufMut, U: IoBufMut>(
        &self,
        code: u32,
        input: T,
        output: U,
    ) -> Ioctl<T, U> {
        // SAFETY: caller ensures IOCTL is safe.
        Ioctl(unsafe { Io::ioctl(self, code, input, output) })
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
        Custom(Io::issue(self, 0, buffers, f))
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
