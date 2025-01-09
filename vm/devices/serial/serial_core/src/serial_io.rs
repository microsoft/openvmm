// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types to help in the implementation and use [`SerialIo`].

use crate::SerialIo;
use futures::io::AsyncRead;
use futures::io::AsyncWrite;
use inspect::InspectMut;
use parking_lot::Mutex;
use std::fmt::Debug;
use std::io;
use std::io::IoSliceMut;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

/// An implementation of [`SerialIo`] for a connected serial port wrapping an
/// implementation of [`AsyncRead`] and [`AsyncWrite`].
pub struct Connected<T>(T);

impl<T> InspectMut for Connected<T> {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond();
    }
}

impl<T: AsyncRead + AsyncWrite + Send> Connected<T> {
    /// Returns a new instance wrapping `t`.
    pub fn new(t: T) -> Self {
        Self(t)
    }

    /// Returns the wrapped value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> SerialIo for Connected<T> {
    fn is_connected(&self) -> bool {
        true
    }

    fn poll_connect(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_disconnect(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Pending
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Connected<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let r = Pin::new(&mut self.get_mut().0).poll_read(cx, buf);
        if matches!(r, Poll::Ready(Ok(0))) {
            Poll::Pending
        } else {
            r
        }
    }

    fn poll_read_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        let r = Pin::new(&mut self.get_mut().0).poll_read_vectored(cx, bufs);
        if matches!(r, Poll::Ready(Ok(0))) {
            Poll::Pending
        } else {
            r
        }
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Connected<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_close(cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write_vectored(cx, bufs)
    }
}

/// Returns a new implementation of [`SerialIo`] wrapping `t`, plus a handle to
/// detach `t` and get it back.
pub fn detachable<T: SerialIo + Unpin>(t: T) -> (DetachableIo<T>, IoDetacher<T>) {
    let inner = Arc::new(Mutex::new(Some(t)));
    (
        DetachableIo {
            inner: inner.clone(),
        },
        IoDetacher { inner },
    )
}

/// An object implementing [`AsyncRead`] or [`AsyncWrite`] whose underlying
/// object can be detached.
///
/// Once the object is detached (via [`IoDetacher::detach`]), reads will return
/// `Ok(0)` (indicating EOF), and writes will fail with
/// [`std::io::ErrorKind::BrokenPipe`].
#[derive(Debug)]
pub struct DetachableIo<T> {
    inner: Arc<Mutex<Option<T>>>,
}

impl<T: InspectMut> InspectMut for DetachableIo<T> {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        self.inner.lock().inspect_mut(req)
    }
}

impl<T> DetachableIo<T> {
    /// Makes an object that's already in the detached state.
    pub fn detached() -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
        }
    }
}

/// A handle used to detach the object from a [`DetachableIo`].
pub struct IoDetacher<T> {
    inner: Arc<Mutex<Option<T>>>,
}

impl<T: SerialIo + Unpin> IoDetacher<T> {
    /// Takes the underlying IO object from the associated [`DetachableIo`].
    pub fn detach(self) -> T {
        self.inner.lock().take().unwrap()
    }
}

impl<T: SerialIo + Unpin> SerialIo for DetachableIo<T> {
    fn is_connected(&self) -> bool {
        self.inner
            .lock()
            .as_ref()
            .is_some_and(|s| s.is_connected())
    }

    fn poll_connect(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut inner = self.inner.lock();
        if let Some(serial) = &mut *inner {
            serial.poll_connect(cx)
        } else {
            Poll::Pending
        }
    }

    fn poll_disconnect(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut inner = self.inner.lock();
        if let Some(serial) = &mut *inner {
            serial.poll_disconnect(cx)
        } else {
            Poll::Ready(Ok(()))
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for DetachableIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut inner = self.inner.lock();
        if let Some(inner) = &mut *inner {
            Pin::new(inner).poll_read(cx, buf)
        } else {
            Poll::Ready(Ok(0))
        }
    }

    fn poll_read_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        let mut inner = self.inner.lock();
        if let Some(inner) = &mut *inner {
            Pin::new(inner).poll_read_vectored(cx, bufs)
        } else {
            Poll::Ready(Ok(0))
        }
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for DetachableIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut inner = self.inner.lock();
        if let Some(inner) = &mut *inner {
            Pin::new(inner).poll_write(cx, buf)
        } else {
            Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut inner = self.inner.lock();
        if let Some(inner) = &mut *inner {
            Pin::new(inner).poll_flush(cx)
        } else {
            Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut inner = self.inner.lock();
        if let Some(inner) = &mut *inner {
            Pin::new(inner).poll_close(cx)
        } else {
            Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let mut inner = self.inner.lock();
        if let Some(inner) = &mut *inner {
            Pin::new(inner).poll_write_vectored(cx, bufs)
        } else {
            Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
        }
    }
}
