// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(windows)]

//! Console relay support using Windows named pipes.

use futures::AsyncRead;
use futures::AsyncWrite;
use futures::FutureExt;
use pal_async::driver::Driver;
use pal_async::pipe::PolledPipe;
use std::path::Path;
use std::pin::Pin;
use std::task::ready;

pub struct WindowsNamedPipeConsole {
    driver: Box<dyn Driver>,
    state: WindowsNamedPipeConsoleState,
}

enum WindowsNamedPipeConsoleState {
    Listening(pal_async::windows::pipe::ListeningPipe),
    Connected(PolledPipe),
}

impl WindowsNamedPipeConsole {
    pub fn new(driver: Box<dyn Driver>, path: &Path) -> std::io::Result<Self> {
        let server = pal_async::windows::pipe::NamedPipeServer::create(path)?;
        let listener = server.accept(&driver)?;
        Ok(Self {
            driver,
            state: WindowsNamedPipeConsoleState::Listening(listener),
        })
    }

    fn poll_connect(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<&mut PolledPipe>> {
        match &mut self.state {
            WindowsNamedPipeConsoleState::Listening(l) => {
                let pipe = ready!(l.poll_unpin(cx))?;
                let pipe = PolledPipe::new(&self.driver, pipe)?;
                self.state = WindowsNamedPipeConsoleState::Connected(pipe);
            }
            WindowsNamedPipeConsoleState::Connected(_) => {}
        }
        let WindowsNamedPipeConsoleState::Connected(pipe) = &mut self.state else {
            unreachable!()
        };
        Ok(pipe).into()
    }
}

impl AsyncRead for WindowsNamedPipeConsole {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let c = ready!(self.poll_connect(cx))?;
        Pin::new(c).poll_read(cx, buf)
    }
}

impl AsyncWrite for WindowsNamedPipeConsole {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let c = ready!(self.poll_connect(cx))?;
        Pin::new(c).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut self.state {
            WindowsNamedPipeConsoleState::Listening(_) => Ok(()).into(),
            WindowsNamedPipeConsoleState::Connected(c) => Pin::new(c).poll_flush(cx),
        }
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut self.state {
            WindowsNamedPipeConsoleState::Listening(_) => Ok(()).into(),
            WindowsNamedPipeConsoleState::Connected(c) => Pin::new(c).poll_close(cx),
        }
    }
}
