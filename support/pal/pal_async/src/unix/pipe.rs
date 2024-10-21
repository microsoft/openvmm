// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unix polled pipe wrapper.

use crate::driver::Driver;
use crate::driver::PollImpl;
use crate::fd::PollFdReady;
use crate::interest::InterestSlot;
use crate::interest::PollEvents;
use crate::socket::PollReady;
use futures::AsyncRead;
use futures::AsyncWrite;
use pal::unix::pipe::set_nonblocking;
use parking_lot::Mutex;
use std::fs::File;
use std::io;
use std::io::Read;
use std::io::Write;
use std::os::unix::prelude::*;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

/// A polled Unix pipe (or other pipe-like file).
pub struct PolledPipe {
    fd_ready: PollImpl<dyn PollFdReady>,
    file: File,
}

impl PolledPipe {
    /// Creates a polled pipe from a file.
    pub fn new(driver: &(impl ?Sized + Driver), file: File) -> io::Result<Self> {
        let fd_ready = driver.new_dyn_fd_ready(file.as_raw_fd())?;
        set_nonblocking(&file, true)?;
        Ok(Self { fd_ready, file })
    }

    /// Creates a connected pair of polled pipes, returning (read pipe, write pipe).
    pub fn pair(driver: &(impl ?Sized + Driver)) -> io::Result<(Self, Self)> {
        let (a, b) = Self::file_pair()?;
        Ok((Self::new(driver, a)?, Self::new(driver, b)?))
    }

    /// Creates a connected pair of pipes (read pipe, write pipe) suitable for
    /// passing to [`Self::new`].
    pub fn file_pair() -> io::Result<(File, File)> {
        pal::unix::pipe::pair()
    }

    /// Returns the inner pipe file.
    pub fn into_inner(self) -> File {
        set_nonblocking(&self.file, false).unwrap();
        self.file
    }

    /// Returns the inner file.
    pub fn get(&self) -> &File {
        &self.file
    }

    /// Splits the file into a read and write half that can be used
    /// concurrently.
    ///
    /// This is more flexible and efficient than
    /// [`futures::io::AsyncReadExt::split`], since it avoids holding a lock
    /// while calling into the kernel, and it provides access to the underlying
    /// file for more advanced operations.
    pub fn split(self) -> (ReadHalf, WriteHalf) {
        let inner = Arc::new(SplitInner {
            fd_ready: Mutex::new(self.fd_ready),
            file: self.file,
        });
        (
            ReadHalf {
                inner: inner.clone(),
            },
            WriteHalf { inner },
        )
    }

    fn poll_io<F, R>(
        &mut self,
        cx: &mut Context<'_>,
        slot: InterestSlot,
        events: PollEvents,
        mut f: F,
    ) -> Poll<io::Result<R>>
    where
        F: FnMut(&mut Self) -> io::Result<R>,
    {
        loop {
            std::task::ready!(self.fd_ready.poll_fd_ready(cx, slot, events));
            match f(self) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    self.fd_ready.clear_fd_ready(slot);
                }
                r => break Poll::Ready(r),
            }
        }
    }
}

impl PollReady for PolledPipe {
    fn poll_ready(&mut self, cx: &mut Context<'_>, events: PollEvents) -> Poll<PollEvents> {
        self.fd_ready.poll_fd_ready(cx, InterestSlot::Read, events)
    }
}

impl AsyncRead for PolledPipe {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_io(cx, InterestSlot::Read, PollEvents::IN, |this| {
            this.file.read(buf)
        })
    }

    fn poll_read_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        self.poll_io(cx, InterestSlot::Read, PollEvents::IN, |this| {
            this.file.read_vectored(bufs)
        })
    }
}

impl AsyncWrite for PolledPipe {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_io(cx, InterestSlot::Write, PollEvents::OUT, |this| {
            this.file.write(buf)
        })
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_io(cx, InterestSlot::Write, PollEvents::OUT, |this| {
            this.file.flush()
        })
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::ErrorKind::Unsupported.into()))
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.poll_io(cx, InterestSlot::Write, PollEvents::OUT, |this| {
            this.file.write_vectored(bufs)
        })
    }
}

struct SplitInner {
    fd_ready: Mutex<PollImpl<dyn PollFdReady>>, // must be first--some executors require that it's dropped before file.
    file: File,
}

/// The read half of a file, via [`PolledPipe::split`].
pub struct ReadHalf {
    inner: Arc<SplitInner>,
}

impl ReadHalf {
    /// Gets a reference to the inner file.
    pub fn get(&self) -> &File {
        &self.inner.file
    }

    /// Calls nonblocking operation `f` when the file is ready for read.
    ///
    /// If `f` returns `Err(err)` with `err.kind() ==
    /// io::ErrorKind::WouldBlock`, then this re-polls the file for readiness
    /// and returns `Poll::Pending`.
    pub fn poll_io<F, R>(&mut self, cx: &mut Context<'_>, mut f: F) -> Poll<io::Result<R>>
    where
        F: FnMut(&mut Self) -> io::Result<R>,
    {
        loop {
            std::task::ready!(self.inner.fd_ready.lock().poll_fd_ready(
                cx,
                InterestSlot::Read,
                PollEvents::IN
            ));
            match f(self) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    self.inner
                        .fd_ready
                        .lock()
                        .clear_fd_ready(InterestSlot::Read);
                }
                r => break Poll::Ready(r),
            }
        }
    }
}

/// The write half of a file, via [`PolledPipe::split`].
pub struct WriteHalf {
    inner: Arc<SplitInner>,
}

impl WriteHalf {
    /// Gets a reference to the inner file.
    pub fn get(&self) -> &File {
        &self.inner.file
    }

    /// Calls nonblocking operation `f` when the file is ready for write.
    ///
    /// If `f` returns `Err(err)` with `err.kind() ==
    /// io::ErrorKind::WouldBlock`, then this re-polls the file for readiness
    /// and returns `Poll::Pending`.
    pub fn poll_io<F, R>(&mut self, cx: &mut Context<'_>, mut f: F) -> Poll<io::Result<R>>
    where
        F: FnMut(&mut Self) -> io::Result<R>,
    {
        loop {
            std::task::ready!(self.inner.fd_ready.lock().poll_fd_ready(
                cx,
                InterestSlot::Write,
                PollEvents::OUT
            ));
            match f(self) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    self.inner
                        .fd_ready
                        .lock()
                        .clear_fd_ready(InterestSlot::Write);
                }
                r => break Poll::Ready(r),
            }
        }
    }
}

impl PollReady for ReadHalf {
    fn poll_ready(&mut self, cx: &mut Context<'_>, events: PollEvents) -> Poll<PollEvents> {
        self.inner
            .fd_ready
            .lock()
            .poll_fd_ready(cx, InterestSlot::Read, events)
    }
}

impl AsyncRead for ReadHalf {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_io(cx, |this| (&this.inner.file).read(buf))
    }

    fn poll_read_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        self.poll_io(cx, |this| (&this.inner.file).read_vectored(bufs))
    }
}

impl PollReady for WriteHalf {
    fn poll_ready(&mut self, cx: &mut Context<'_>, events: PollEvents) -> Poll<PollEvents> {
        self.inner
            .fd_ready
            .lock()
            .poll_fd_ready(cx, InterestSlot::Write, events)
    }
}

impl AsyncWrite for WriteHalf {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_io(cx, |this| (&this.inner.file).write(buf))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_io(cx, |this| (&this.inner.file).flush())
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::ErrorKind::Unsupported.into()))
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.poll_io(cx, |this| (&this.inner.file).write_vectored(bufs))
    }
}
