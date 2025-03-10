// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Traits for async send and receive of datagrams.
//!
//! Datagrams are self-contained messages that are not split or combined when
//! sent or received and instead always maintain their original message
//! boundaries.
//!
//! This is different from bytes sent or received over a byte stream (as in
//! [`futures::AsyncRead`]), where one send can be split into multiple receives,
//! or multiple sends can be combined into one receive.

use std::io;
use std::io::IoSlice;
use std::io::IoSliceMut;
use std::pin::Pin;
use std::task::ready;
use std::task::Context;
use std::task::Poll;
use thiserror::Error;

/// Trait implemented by types that can receive datagrams.
///
/// This is different from [`futures::AsyncRead`], which is used for byte
/// streams.
pub trait AsyncRecv {
    /// Polls for an incoming datagram, which will be gathered into `bufs`.
    ///
    /// At most one datagram will be received per call.
    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>>;
}

impl<T: AsyncRecv + ?Sized> AsyncRecv for &mut T {
    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        (*self).poll_recv(cx, bufs)
    }
}

/// Extension trait for [`AsyncRecv`].
pub trait AsyncRecvExt: AsyncRecv {
    /// Receive a datagram into `buf`.
    fn recv<'a>(&'a mut self, buf: &'a mut [u8]) -> Recv<'a, Self> {
        Recv { recv: self, buf }
    }

    /// Receive a datagram into `buf`, failing if its size is not exactly the
    /// size of `buf`.
    fn recv_exact<'a>(&'a mut self, buf: &'a mut [u8]) -> RecvExact<'a, Self> {
        RecvExact { recv: self, buf }
    }

    /// Read a single datagram into `bufs`.
    ///
    /// Slice will be written in order, with the next one used only after the
    /// previous one is completely filled.
    fn recv_vectored<'a>(&'a mut self, bufs: &'a mut [IoSliceMut<'a>]) -> RecvVectored<'a, Self> {
        RecvVectored { recv: self, bufs }
    }
}

impl<T: AsyncRecv + ?Sized> AsyncRecvExt for T {}

/// A future for [`AsyncRecvExt::recv`].
pub struct Recv<'a, T: ?Sized> {
    recv: &'a mut T,
    buf: &'a mut [u8],
}

impl<T: AsyncRecv + ?Sized> Future for Recv<'_, T> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.recv.poll_recv(cx, &mut [IoSliceMut::new(this.buf)])
    }
}

/// A future for [`AsyncRecvExt::recv_exact`].
pub struct RecvExact<'a, T: ?Sized> {
    recv: &'a mut T,
    buf: &'a mut [u8],
}

#[derive(Debug, Error)]
#[error("message too small")]
struct MessageTooSmall;

impl<T: AsyncRecv + ?Sized> Future for RecvExact<'_, T> {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let n = ready!(this.recv.poll_recv(cx, &mut [IoSliceMut::new(this.buf)]))?;
        if n != this.buf.len() {
            Err(io::Error::new(io::ErrorKind::InvalidData, MessageTooSmall))?;
        }
        Poll::Ready(Ok(()))
    }
}

/// A future for [`AsyncRecvExt::recv_vectored`].
pub struct RecvVectored<'a, T: ?Sized> {
    recv: &'a mut T,
    bufs: &'a mut [IoSliceMut<'a>],
}

impl<T: AsyncRecv + ?Sized> Future for RecvVectored<'_, T> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.recv.poll_recv(cx, this.bufs)
    }
}

/// A trait implemented by types that can send datagrams.
pub trait AsyncSend {
    /// Polls to send a datagram given by `bufs`.
    ///
    /// There are no partial sends--either the datagram is sent or it is not.
    fn poll_send(&mut self, cx: &mut Context<'_>, bufs: &[IoSlice<'_>]) -> Poll<io::Result<()>>;
}

impl<T: AsyncSend + ?Sized> AsyncSend for &mut T {
    fn poll_send(&mut self, cx: &mut Context<'_>, bufs: &[IoSlice<'_>]) -> Poll<io::Result<()>> {
        (*self).poll_send(cx, bufs)
    }
}

/// Extension trait for [`AsyncSend`].
pub trait AsyncSendExt: AsyncSend {
    /// Sends the datagram in `buf`.
    fn send<'a>(&'a mut self, buf: &'a [u8]) -> Send<'a, Self> {
        Send { send: self, buf }
    }

    /// Sends the datagram in `bufs`.
    fn send_vectored<'a>(&'a mut self, bufs: &'a [IoSlice<'a>]) -> SendVectored<'a, Self> {
        SendVectored { send: self, bufs }
    }
}

impl<T: AsyncSend + ?Sized> AsyncSendExt for T {}

/// A future for [`AsyncSendExt::send`].
pub struct Send<'a, T: ?Sized> {
    send: &'a mut T,
    buf: &'a [u8],
}

impl<T: AsyncSend + ?Sized> Future for Send<'_, T> {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.send.poll_send(cx, &[IoSlice::new(this.buf)])
    }
}

/// A future for [`AsyncSendExt::send_vectored`].
pub struct SendVectored<'a, T: ?Sized> {
    send: &'a mut T,
    bufs: &'a [IoSlice<'a>],
}

impl<T: AsyncSend + ?Sized> Future for SendVectored<'_, T> {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.send.poll_send(cx, this.bufs)
    }
}
