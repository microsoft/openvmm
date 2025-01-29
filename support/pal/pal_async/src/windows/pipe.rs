// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows polled pipe wrapper.

use crate::driver::Driver;
use crate::driver::PollImpl;
use crate::interest::InterestSlot;
use crate::interest::SLOT_COUNT;
use crate::multi_waker::MultiWaker;
use crate::wait::PollWait;
use crate::wait::PolledWait;
use futures::AsyncRead;
use futures::AsyncWrite;
use pal::windows::chk_status;
use pal::windows::pipe::new_named_pipe;
use pal::windows::pipe::Disposition;
use pal::windows::pipe::PipeExt;
use pal::windows::pipe::PipeMode;
use pal::windows::pipe::FILE_PIPE_DISCONNECTED;
use pal::windows::pipe::FILE_PIPE_READ_READY;
use pal::windows::pipe::FILE_PIPE_WRITE_READY;
use pal::windows::Overlapped;
use pal_event::Event;
use std::fs::File;
use std::future::Future;
use std::io;
use std::io::Write;
use std::os::windows::prelude::*;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::ptr::null_mut;
use std::task::ready;
use std::task::Context;
use std::task::Poll;
use winapi::shared::winerror::ERROR_BROKEN_PIPE;
use winapi::shared::winerror::ERROR_IO_PENDING;
use winapi::shared::winerror::ERROR_NO_DATA;
use winapi::shared::winerror::ERROR_PIPE_CONNECTED;
use winapi::shared::winerror::ERROR_PIPE_NOT_CONNECTED;
use winapi::um::fileapi::ReadFile;
use winapi::um::ioapiset::CancelIoEx;
use winapi::um::namedpipeapi::ConnectNamedPipe;
use winapi::um::winbase::PIPE_NOWAIT;
use winapi::um::winbase::PIPE_READMODE_MESSAGE;
use winapi::um::winbase::PIPE_WAIT;
use winapi::um::winnt::GENERIC_READ;
use winapi::um::winnt::GENERIC_WRITE;

/// A Windows pipe, configured for polled IO.
pub struct PolledPipe {
    wait: PollImpl<dyn PollWait>,
    wakers: MultiWaker<SLOT_COUNT>,
    file: File,
    _event: Event,
    message_mode: bool,
    out_buffer_size: u32,
    events: u32,
}

impl PolledPipe {
    /// Configures a pipe file for polled use.
    ///
    /// Due to platform limitations, this will fail for unidirectional pipes and unbuffered pipes.
    pub fn new(driver: &(impl ?Sized + Driver), file: File) -> io::Result<Self> {
        let message_mode = file.get_pipe_state()? & PIPE_READMODE_MESSAGE != 0;
        Self::new_internal(driver, file, message_mode)
    }

    /// Creates a connected pair of polled pipes, returning (read pipe, write pipe).
    pub fn pair(driver: &(impl ?Sized + Driver)) -> io::Result<(Self, Self)> {
        let (a, b) = Self::file_pair()?;
        Ok((Self::new(driver, a)?, Self::new(driver, b)?))
    }

    /// Creates a connected pair of pipes (read pipe, write pipe) suitable for
    /// passing to [`Self::new`].
    pub fn file_pair() -> io::Result<(File, File)> {
        pal::windows::pipe::bidirectional_pair(false)
    }

    /// Returns the inner pipe file.
    pub fn into_inner(self) -> File {
        self.file
            .set_pipe_mode(PIPE_WAIT)
            .expect("unexpected failure restoring pipe mode");
        self.file
    }

    fn new_internal(
        driver: &(impl ?Sized + Driver),
        file: File,
        message_mode: bool,
    ) -> io::Result<Self> {
        let (_, out_buffer_size) = file.get_pipe_buffer_sizes()?;
        if out_buffer_size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unbuffered pipes cannot be polled",
            ));
        }

        let event = Event::new();
        let wait = driver.new_dyn_wait(event.as_handle().as_raw_handle())?;

        let mut mode = PIPE_NOWAIT;
        if message_mode {
            mode |= PIPE_READMODE_MESSAGE;
        }
        file.set_pipe_mode(mode)?;

        match file.set_pipe_select_event(
            &event,
            FILE_PIPE_READ_READY | FILE_PIPE_WRITE_READY | FILE_PIPE_DISCONNECTED,
        ) {
            Ok(()) => {}
            Err(err) if err.raw_os_error() == Some(ERROR_PIPE_NOT_CONNECTED as i32) => {
                // The event could not be registered since the pipe has already
                // disconnected. This is fine to ignore since the event state
                // starts as read+write ready, and reads and writes will both
                // return failures.
            }
            Err(err) => return Err(err),
        }

        Ok(Self {
            file,
            wait,
            wakers: MultiWaker::new(),
            _event: event,
            message_mode,
            out_buffer_size,
            events: FILE_PIPE_READ_READY | FILE_PIPE_WRITE_READY,
        })
    }

    /// Polls the pipe for entering the closing state, where the client has
    /// closed its handle.
    pub fn poll_closing(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.events & FILE_PIPE_DISCONNECTED != 0 {
            // Make sure the pipe is still disconnected.
            self.refresh_events()?;
        }
        while self.events & FILE_PIPE_DISCONNECTED == 0 {
            ready!(self
                .wakers
                .poll_wrapped(cx, InterestSlot::Read as usize, |cx| self
                    .wait
                    .poll_wait(cx)))?;

            self.refresh_events()?;
        }
        Poll::Ready(Ok(()))
    }

    fn is_read_ready(&self) -> bool {
        self.events & (FILE_PIPE_READ_READY | FILE_PIPE_DISCONNECTED) != 0
    }

    fn is_write_ready(&self) -> bool {
        self.events & (FILE_PIPE_WRITE_READY | FILE_PIPE_DISCONNECTED) != 0
    }

    /// Refreshes the current event state from the pipe.
    fn refresh_events(&mut self) -> io::Result<()> {
        // Capture the current event state.
        self.events = self.file.get_pipe_select_events()?;
        Ok(())
    }
}

/// Like File::read except it doesn't translate ERROR_NO_DATA to Ok(0) (or any
/// error codes for that matter).
fn read_file(file: &File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    // SAFETY: calling API as documented, with owned buffer of correct length.
    let r = unsafe {
        ReadFile(
            file.as_raw_handle(),
            buf.as_mut_ptr().cast(),
            buf.len().try_into().unwrap_or(u32::MAX),
            &mut n,
            null_mut(),
        )
    };
    if r != 0 {
        Ok(n as usize)
    } else {
        Err(io::Error::last_os_error())
    }
}

impl AsyncRead for PolledPipe {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let n = loop {
            while !this.is_read_ready() {
                ready!(this
                    .wakers
                    .poll_wrapped(cx, InterestSlot::Read as usize, |cx| this
                        .wait
                        .poll_wait(cx)))?;

                this.refresh_events()?;
            }
            match read_file(&this.file, buf) {
                Ok(n) => {
                    if n < buf.len() && !this.message_mode {
                        this.events &= !FILE_PIPE_READ_READY;
                    }
                    break n;
                }
                Err(err)
                    if matches!(
                        err.raw_os_error().map(|v| v as u32),
                        Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED)
                    ) =>
                {
                    // ERROR_BROKEN_PIPE is returned when the handle is closed.
                    // ERROR_PIPE_NOT_CONNECTED is returned when the server
                    // explicltly calls DisconnectNamedPipe. Either way, the
                    // pipe is closed, so treat it as EOF.
                    //
                    // Note that in either case there may have been data loss,
                    // since Windows named pipes drop all queued data when one
                    // endpoint closes the pipe. It's not possible to detect
                    // this, so there is no way to distinguish between clean and
                    // unclean close.
                    //
                    // (Well, there is a trick, which is to put the pipe into
                    // message mode and send a zero-length message to the other
                    // end before closing. That operating mode not currently
                    // supported by this crate.)
                    break 0;
                }
                Err(err) if err.raw_os_error() == Some(ERROR_NO_DATA as i32) => {
                    this.events &= !FILE_PIPE_READ_READY;
                }
                Err(err) => return Poll::Ready(Err(err)),
            }
        };
        Poll::Ready(Ok(n))
    }
}

impl AsyncWrite for PolledPipe {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        // Limit the buffer to fit in the pipe's output buffer. Otherwise the
        // write will always return zero.
        let buf = &buf[..buf.len().min(this.out_buffer_size as usize)];

        let n = loop {
            while !this.is_write_ready() {
                ready!(this
                    .wakers
                    .poll_wrapped(cx, InterestSlot::Write as usize, |cx| this
                        .wait
                        .poll_wait(cx)))?;
                this.refresh_events()?;
            }
            let n = this.file.write(buf)?;
            if n > 0 || buf.is_empty() {
                break n;
            }
            this.events &= !FILE_PIPE_WRITE_READY;
        };
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::ErrorKind::Unsupported.into()))
    }
}

/// A named pipe server.
#[derive(Debug)]
pub struct NamedPipeServer {
    _root_pipe: File,
    path: PathBuf,
}

impl NamedPipeServer {
    /// Creates a new server at `path`.
    pub fn create(path: impl AsRef<Path>) -> io::Result<Self> {
        // Open the pipe with no access to reserve the name without providing a
        // pipe for clients to connect to. This is needed so that a client
        // connection blocks until the server is actually ready.
        let root_pipe = new_named_pipe(path.as_ref(), 0, Disposition::Create, PipeMode::Byte)?;
        Ok(Self {
            _root_pipe: root_pipe,
            path: path.as_ref().to_owned(),
        })
    }

    fn new_pipe(&self) -> io::Result<File> {
        new_named_pipe(
            &self.path,
            GENERIC_READ | GENERIC_WRITE,
            Disposition::Open,
            PipeMode::Byte,
        )
    }

    /// Initiates an accept, allowing a client to connect.
    pub fn accept(&self, driver: &(impl ?Sized + Driver)) -> io::Result<ListeningPipe> {
        ListeningPipe::new(driver, self.new_pipe()?)
    }
}

/// A named pipe in the listening state, waiting for a client to connect.
#[must_use]
pub struct ListeningPipe {
    inner: Option<ListeningInner>,
}

struct ListeningInner {
    file: File,
    event: PolledWait<Event>,
    overlapped: Box<Overlapped>,
    sync_success: bool,
}

impl Drop for ListeningPipe {
    fn drop(&mut self) {
        // Ensure the IO is complete before freeing the overlapped structure.
        self.cancel_and_wait();
    }
}

impl ListeningPipe {
    /// Makes a new listening pipe.
    ///
    /// The pipe file must have been opened for overlapped IO or this will hang.
    ///
    /// This will fail if the pipe is in the closing state. Use
    /// [`PipeExt::disconnect_pipe`] to disconnect a pipe that the client has
    /// closed.
    pub fn new(driver: &(impl Driver + ?Sized), pipe: File) -> io::Result<Self> {
        let event = PolledWait::new(driver, Event::new())?;
        let mut overlapped = Box::new(Overlapped::default());
        overlapped.set_event(event.get().as_handle().as_raw_handle());

        // SAFETY: drop() and into_inner() ensure that the overlapped object is
        // only deallocated after the IO completes.
        let success = unsafe { ConnectNamedPipe(pipe.as_raw_handle(), overlapped.as_ptr()) };
        let sync_success = if success != 0 {
            true
        } else {
            let err = io::Error::last_os_error();
            match err.raw_os_error().unwrap() as u32 {
                ERROR_PIPE_CONNECTED => {
                    // The pipe is already connected, consider that a success.
                    true
                }
                ERROR_IO_PENDING => false,
                _ => return Err(err),
            }
        };
        Ok(Self {
            inner: Some(ListeningInner {
                file: pipe,
                event,
                overlapped,
                sync_success,
            }),
        })
    }

    /// Returns the inner file, whether or not the connection completed yet.
    pub fn into_inner(mut self) -> File {
        self.cancel_and_wait().unwrap()
    }

    fn cancel_and_wait(&mut self) -> Option<File> {
        let inner = self.inner.take()?;
        if !inner.sync_success && inner.overlapped.io_status().is_none() {
            let event = inner.event.into_inner();
            if inner.overlapped.io_status().is_none() {
                // SAFETY: calling as documented.
                unsafe { CancelIoEx(inner.file.as_raw_handle(), inner.overlapped.as_ptr()) };
                event.wait();
                assert!(inner.overlapped.io_status().is_some());
            }
        }
        Some(inner.file)
    }
}

impl Future for ListeningPipe {
    type Output = io::Result<File>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let inner = this.inner.as_mut().expect("polled after completion");
        ready!(inner.event.poll_wait(cx))?;
        let (status, _) = inner.overlapped.io_status().expect("io should be complete");
        chk_status(status)?;
        Poll::Ready(Ok(this.inner.take().unwrap().file))
    }
}

#[cfg(test)]
mod tests {
    use super::PolledPipe;
    use crate::sys::pipe::NamedPipeServer;
    use crate::DefaultDriver;
    use futures::AsyncReadExt;
    use futures::AsyncWriteExt;
    use pal_async_test::async_test;
    use std::fs::OpenOptions;

    #[async_test]
    async fn named_pipe_server(driver: DefaultDriver) {
        let mut path = [0; 16];
        getrandom::getrandom(&mut path).unwrap();
        let path = format!(r#"\\.\pipe\{:0x}"#, u128::from_ne_bytes(path));
        let server = NamedPipeServer::create(&path).unwrap();
        let mut c;
        let mut s;
        let mut i = 0i32;
        loop {
            let mut accept = server.accept(&driver).unwrap();
            assert!(futures::poll!(&mut accept).is_pending());
            c = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();

            s = accept.await.unwrap();
            if i == 3 {
                break;
            }
            i += 1;
        }

        let mut c = PolledPipe::new(&driver, c).unwrap();
        let mut s = PolledPipe::new(&driver, s).unwrap();
        s.write_all(b"abc").await.unwrap();
        drop(s);
        let mut b = vec![];
        c.read_to_end(&mut b).await.unwrap();
        assert_eq!(b.as_slice(), b"abc");
    }

    #[async_test]
    async fn half_open(driver: DefaultDriver) {
        let (p1, p2) = pal::windows::pipe::bidirectional_pair(false).unwrap();
        drop(p2);
        let mut p1 = PolledPipe::new(&driver, p1).unwrap();
        let mut b = [0];
        assert_eq!(p1.read(&mut b).await.unwrap(), 0);
    }
}
