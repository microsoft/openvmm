// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Asynchronous writes to a TAP interface, used by the queue transmit path to
//! wait for the interface to accept a packet.

use super::PolledTap;
use futures::AsyncWrite;
use std::io;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

impl AsyncWrite for PolledTap {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.tap).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.tap).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.tap).poll_close(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.tap).poll_write_vectored(cx, bufs)
    }
}
