// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Socket serial backend, usable for both TCP and Unix sockets (even on
//! Windows).

use futures::AsyncRead;
use futures::AsyncWrite;
use inspect::InspectMut;
use mesh::MeshPayload;
use pal_async::driver::Driver;
use pal_async::interest::PollEvents;
use pal_async::socket::PollReady;
use pal_async::socket::PolledSocket;
use serial_core::SerialIo;
use serial_core::resources::ResolveSerialBackendParams;
use serial_core::resources::ResolvedSerialBackend;
use socket2::Socket;
use std::io;
use std::net::TcpListener;
use std::net::TcpStream;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::task::ready;
use unix_socket::UnixListener;
use unix_socket::UnixStream;
use vm_resource::ResolveResource;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::declare_static_resolver;
use vm_resource::kind::SerialBackendHandle;

#[derive(Debug, MeshPayload)]
pub struct OpenSocketSerialConfig {
    pub current: Option<Socket>,
    pub listener: Option<Socket>,
}

impl ResourceId<SerialBackendHandle> for OpenSocketSerialConfig {
    const ID: &'static str = "socket";
}

pub struct SocketSerialResolver;
declare_static_resolver!(
    SocketSerialResolver,
    (SerialBackendHandle, OpenSocketSerialConfig)
);

impl ResolveResource<SerialBackendHandle, OpenSocketSerialConfig> for SocketSerialResolver {
    type Output = ResolvedSerialBackend;
    type Error = io::Error;

    fn resolve(
        &self,
        rsrc: OpenSocketSerialConfig,
        input: ResolveSerialBackendParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(SocketSerialBackend::new(input.driver, rsrc)?.into())
    }
}

impl From<UnixStream> for OpenSocketSerialConfig {
    fn from(stream: UnixStream) -> Self {
        Self {
            current: Some(stream.into()),
            listener: None,
        }
    }
}

impl From<UnixListener> for OpenSocketSerialConfig {
    fn from(listener: UnixListener) -> Self {
        Self {
            current: None,
            listener: Some(listener.into()),
        }
    }
}

impl From<TcpStream> for OpenSocketSerialConfig {
    fn from(stream: TcpStream) -> Self {
        Self {
            current: Some(stream.into()),
            listener: None,
        }
    }
}

impl From<TcpListener> for OpenSocketSerialConfig {
    fn from(listener: TcpListener) -> Self {
        Self {
            current: None,
            listener: Some(listener.into()),
        }
    }
}

pub struct SocketSerialBackend {
    driver: Box<dyn Driver>,
    current: Option<PolledSocket<Socket>>,
    listener: Option<PolledSocket<Socket>>,
}

impl InspectMut for SocketSerialBackend {
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

impl SocketSerialBackend {
    pub fn new(driver: Box<dyn Driver>, config: OpenSocketSerialConfig) -> io::Result<Self> {
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

    pub fn into_config(self) -> OpenSocketSerialConfig {
        OpenSocketSerialConfig {
            current: self.current.map(PolledSocket::into_inner),
            listener: self.listener.map(PolledSocket::into_inner),
        }
    }
}

impl From<SocketSerialBackend> for Resource<SerialBackendHandle> {
    fn from(value: SocketSerialBackend) -> Self {
        Resource::new(value.into_config())
    }
}

impl SerialIo for SocketSerialBackend {
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

impl AsyncRead for SocketSerialBackend {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let Some(current) = &mut self.current else {
            return Poll::Ready(Ok(0));
        };
        match ready!(Pin::new(current).poll_read(cx, buf)) {
            Ok(0) => {}
            Ok(n) => return Poll::Ready(Ok(n)),
            Err(err) => {
                tracing::warn!(
                    error = &err as &dyn std::error::Error,
                    "serial socket read failed, disconnecting"
                );
            }
        }
        self.current = None;
        // SerialIo reports transport disconnects as EOF.
        Poll::Ready(Ok(0))
    }
}

impl AsyncWrite for SocketSerialBackend {
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::AsyncReadExt;
    use futures::future::poll_fn;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use std::io::Write;
    use std::net::SocketAddr;
    use std::time::Duration;
    use test_with_tracing::test;

    fn new_backend(driver: DefaultDriver) -> (SocketSerialBackend, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let backend =
            SocketSerialBackend::new(Box::new(driver), OpenSocketSerialConfig::from(listener))
                .unwrap();
        (backend, address)
    }

    async fn connect_peer(backend: &mut SocketSerialBackend, address: SocketAddr) -> TcpStream {
        let peer = TcpStream::connect(address).unwrap();
        poll_fn(|cx| backend.poll_connect(cx)).await.unwrap();
        assert!(backend.is_connected());
        peer
    }

    fn reset_peer(peer: TcpStream) {
        let peer = Socket::from(peer);
        peer.set_linger(Some(Duration::ZERO)).unwrap();
        drop(peer);
    }

    #[async_test]
    async fn read_returns_input_then_eof_and_accepts_replacement(driver: DefaultDriver) {
        let (mut backend, address) = new_backend(driver);
        let mut peer = connect_peer(&mut backend, address).await;
        peer.write_all(b"input").unwrap();
        drop(peer);

        let mut buf = [0; 5];
        backend.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"input");
        assert_eq!(backend.read(&mut buf).await.unwrap(), 0);
        assert!(!backend.is_connected());

        let _replacement = connect_peer(&mut backend, address).await;
    }

    #[async_test]
    async fn read_failure_returns_eof_and_accepts_replacement(driver: DefaultDriver) {
        let (mut backend, address) = new_backend(driver);
        let peer = connect_peer(&mut backend, address).await;
        reset_peer(peer);

        let mut buf = [0];
        assert_eq!(backend.read(&mut buf).await.unwrap(), 0);
        assert!(!backend.is_connected());
        assert_eq!(backend.read(&mut buf).await.unwrap(), 0);

        let _replacement = connect_peer(&mut backend, address).await;
    }
}
