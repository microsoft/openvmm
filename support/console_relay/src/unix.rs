// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(unix)]

//! Console relay support using Unix domain sockets.

use futures::AsyncRead;
use futures::AsyncWrite;
use pal_async::driver::Driver;
use pal_async::socket::PolledSocket;
use std::path::Path;
use std::pin::Pin;
use std::task::ready;
use std::task::Context;
use unix_socket::UnixListener;
use unix_socket::UnixStream;

pub struct UnixSocketConsole {
    driver: Box<dyn Driver>,
    state: UnixSocketConsoleState,
}

enum UnixSocketConsoleState {
    Listening(PolledSocket<UnixListener>),
    Connected(PolledSocket<UnixStream>),
}

impl UnixSocketConsole {
    pub fn new(driver: Box<dyn Driver>, path: &Path) -> std::io::Result<Self> {
        let listener = UnixListener::bind(path)?;
        Ok(Self {
            state: UnixSocketConsoleState::Listening(PolledSocket::new(&driver, listener)?),
            driver,
        })
    }

    fn poll_connect(
        &mut self,
        cx: &mut Context<'_>,
    ) -> std::task::Poll<std::io::Result<&mut PolledSocket<UnixStream>>> {
        match &mut self.state {
            UnixSocketConsoleState::Listening(l) => {
                let (c, _) = ready!(l.poll_accept(cx))?;
                let c = PolledSocket::new(&self.driver, c)?;
                self.state = UnixSocketConsoleState::Connected(c);
            }
            UnixSocketConsoleState::Connected(_) => {}
        }
        let UnixSocketConsoleState::Connected(c) = &mut self.state else {
            unreachable!()
        };
        Ok(c).into()
    }
}

impl AsyncRead for UnixSocketConsole {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let c = ready!(self.poll_connect(cx))?;
        Pin::new(c).poll_read(cx, buf)
    }
}

impl AsyncWrite for UnixSocketConsole {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let c = ready!(self.poll_connect(cx))?;
        Pin::new(c).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut self.state {
            UnixSocketConsoleState::Listening(_) => Ok(()).into(),
            UnixSocketConsoleState::Connected(c) => Pin::new(c).poll_flush(cx),
        }
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut self.state {
            UnixSocketConsoleState::Listening(_) => Ok(()).into(),
            UnixSocketConsoleState::Connected(c) => Pin::new(c).poll_close(cx),
        }
    }
}
