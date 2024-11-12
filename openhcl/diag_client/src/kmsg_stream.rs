// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types for handling a kmsg byte stream, which is a series of kmsg entries
//! separated by null terminators.

use diag_proto::FILE_LINE_MAX;
use futures::AsyncRead;
use pal_async::socket::PolledSocket;
use std::io;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

/// A stream of data from a /dev/kmsg device, whose contents are defined to have
/// distinct entries separated by null bytes.
pub struct KmsgStream {
    socket: PolledSocket<socket2::Socket>,
    buffer: Vec<u8>,
    end: usize,
}

impl KmsgStream {
    pub(crate) fn new(socket: PolledSocket<socket2::Socket>) -> Self {
        Self {
            socket,
            buffer: vec![0; FILE_LINE_MAX],
            end: 0,
        }
    }
}

impl futures::Stream for KmsgStream {
    type Item = io::Result<Vec<u8>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        // The entries are separated by null terminators. Read until we
        // find a null terminator.
        loop {
            if let Some(len) = this.buffer[..this.end].iter().position(|&x| x == 0) {
                let line = this.buffer[..len].to_vec();
                this.buffer.copy_within(len + 1..this.end, 0);
                this.end -= len + 1;
                break Poll::Ready(Some(Ok(line)));
            } else if this.end == this.buffer.len() {
                return Poll::Ready(Some(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "missing null terminator",
                ))));
            } else {
                match std::task::ready!(
                    Pin::new(&mut this.socket).poll_read(cx, &mut this.buffer[this.end..])
                ) {
                    Ok(n) => {
                        if n == 0 {
                            break Poll::Ready(None);
                        }
                        this.end += n
                    }
                    Err(err) => return Poll::Ready(Some(Err(err))),
                }
            }
        }
    }
}
