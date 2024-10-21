// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unix stream socket serial backend.
//!
//! Despite the name, this is available on Windows, too.

use futures::AsyncRead;
use futures::AsyncWrite;
use inspect::InspectMut;
use mesh::MeshPayload;
use pal_async::driver::Driver;
use pal_async::interest::PollEvents;
use pal_async::socket::PollReady;
use pal_async::socket::PolledSocket;
use serial_core::resources::ResolveSerialBackendParams;
use serial_core::resources::ResolvedSerialBackend;
use serial_core::SerialIo;
use std::io;
use std::pin::Pin;
use std::task::ready;
use std::task::Context;
use std::task::Poll;
use unix_socket::UnixListener;
use unix_socket::UnixStream;
use vm_resource::declare_static_resolver;
use vm_resource::kind::SerialBackendHandle;
use vm_resource::ResolveResource;
use vm_resource::Resource;
use vm_resource::ResourceId;

#[derive(Debug, MeshPayload)]
pub struct OpenUnixStreamSerialConfig {
    pub current: Option<UnixStream>,
    pub listener: Option<UnixListener>,
}

impl ResourceId<SerialBackendHandle> for OpenUnixStreamSerialConfig {
    const ID: &'static str = "unix_socket";
}

pub struct UnixStreamSerialResolver;
declare_static_resolver!(
    UnixStreamSerialResolver,
    (SerialBackendHandle, OpenUnixStreamSerialConfig)
);

impl ResolveResource<SerialBackendHandle, OpenUnixStreamSerialConfig> for UnixStreamSerialResolver {
    type Output = ResolvedSerialBackend;
    type Error = io::Error;

    fn resolve(
        &self,
        rsrc: OpenUnixStreamSerialConfig,
        input: ResolveSerialBackendParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(UnixStreamSerialBackend::new(input.driver, rsrc)?.into())
    }
}

impl From<UnixStream> for OpenUnixStreamSerialConfig {
    fn from(stream: UnixStream) -> Self {
        Self {
            current: Some(stream),
            listener: None,
        }
    }
}

impl From<UnixListener> for OpenUnixStreamSerialConfig {
    fn from(listener: UnixListener) -> Self {
        Self {
            current: None,
            listener: Some(listener),
        }
    }
}

pub struct UnixStreamSerialBackend {
    driver: Box<dyn Driver>,
    current: Option<PolledSocket<UnixStream>>,
    listener: Option<PolledSocket<UnixListener>>,
}

impl InspectMut for UnixStreamSerialBackend {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond().field_with("state", || {
            if self.current.is_some() {
                "connected"
            } else if self.listener.is_some() {
                "listening"
            } else {
                "done"
            }
        });
    }
}

impl UnixStreamSerialBackend {
    pub fn new(driver: Box<dyn Driver>, config: OpenUnixStreamSerialConfig) -> io::Result<Self> {
        let current = config
            .current
            .map(|s| PolledSocket::new(&driver, s))
            .transpose()?;
        let listener = config
            .listener
            .map(|s| PolledSocket::new(&driver, s))
            .transpose()?;
        Ok(Self {
            driver: Box::new(driver),
            current,
            listener,
        })
    }

    pub fn into_config(self) -> OpenUnixStreamSerialConfig {
        OpenUnixStreamSerialConfig {
            current: self.current.map(PolledSocket::into_inner),
            listener: self.listener.map(PolledSocket::into_inner),
        }
    }
}

impl From<UnixStreamSerialBackend> for Resource<SerialBackendHandle> {
    fn from(value: UnixStreamSerialBackend) -> Self {
        Resource::new(value.into_config())
    }
}

impl SerialIo for UnixStreamSerialBackend {
    fn is_connected(&self) -> bool {
        self.current.is_some()
    }

    fn poll_connect(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.current.is_some() {
            Poll::Ready(Ok(()))
        } else if let Some(listener) = &mut self.listener {
            let (socket, _) = ready!(listener.poll_accept(cx))?;
            self.current = Some(PolledSocket::new(&self.driver, socket)?);
            Poll::Ready(Ok(()))
        } else {
            // This will never complete.
            Poll::Pending
        }
    }

    fn poll_disconnect(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(current) = &mut self.current {
            ready!(current.poll_ready(cx, PollEvents::RDHUP));
            self.current = None;
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for UnixStreamSerialBackend {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let Some(current) = &mut self.current else {
            return Poll::Ready(Ok(0));
        };
        let r = ready!(Pin::new(current).poll_read(cx, buf));
        if matches!(r, Ok(0)) {
            self.current = None;
        }
        Poll::Ready(r)
    }
}

impl AsyncWrite for UnixStreamSerialBackend {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let Some(current) = &mut self.current else {
            return Poll::Ready(Ok(buf.len()));
        };
        let r = ready!(Pin::new(current).poll_write(cx, buf));
        if matches!(&r, Err(err) if err.kind() == io::ErrorKind::BrokenPipe) {
            return Poll::Ready(Ok(buf.len()));
        }
        Poll::Ready(r)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Some(current) = &mut self.current else {
            return Poll::Ready(Ok(()));
        };
        let r = ready!(Pin::new(current).poll_flush(cx));
        if matches!(&r, Err(err) if err.kind() == io::ErrorKind::BrokenPipe) {
            return Poll::Ready(Ok(()));
        }
        Poll::Ready(r)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Some(current) = &mut self.current else {
            return Poll::Ready(Ok(()));
        };
        let r = ready!(Pin::new(current).poll_close(cx));
        if matches!(&r, Err(err) if err.kind() == io::ErrorKind::BrokenPipe) {
            return Poll::Ready(Ok(()));
        }
        Poll::Ready(r)
    }
}
