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
use std::mem::MaybeUninit;
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
    /// Set when a write to `current` failed while input from the peer was
    /// still queued on it. Output is discarded, and `current` is dropped once
    /// reads have delivered that input and hit EOF or an error.
    draining: bool,
}

impl InspectMut for SocketSerialBackend {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond().field_with("state", || {
            if self.draining {
                "draining"
            } else if self.current.is_some() {
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
            draining: false,
        })
    }

    pub fn into_config(self) -> OpenSocketSerialConfig {
        OpenSocketSerialConfig {
            current: self.current.map(PolledSocket::into_inner),
            listener: self.listener.map(PolledSocket::into_inner),
        }
    }

    fn disconnect(&mut self) {
        self.current = None;
        self.draining = false;
    }

    /// Returns the connection to send output to, if any.
    fn output(&mut self) -> Option<&mut PolledSocket<Socket>> {
        if self.draining {
            None
        } else {
            self.current.as_mut()
        }
    }

    /// Handles a failed write, flush, or close on the current connection.
    ///
    /// The peer can no longer receive output, but input it sent before going
    /// away may still be queued on the socket (Linux keeps it readable after a
    /// reset, for example). If so, keep the connection so that reads deliver
    /// that input before reporting EOF, and discard output until then.
    /// Otherwise, drop the connection and return the error, so that the caller
    /// sees the disconnect and `poll_connect` can accept a new client.
    fn handle_write_error(&mut self, err: io::Error) -> io::Result<()> {
        let mut byte = [MaybeUninit::uninit()];
        let input_queued = self
            .current
            .as_ref()
            .is_some_and(|current| matches!(current.get().peek(&mut byte), Ok(n) if n > 0));
        if input_queued {
            tracing::warn!(
                error = &err as &dyn std::error::Error,
                "serial socket write failed, draining queued input before disconnecting"
            );
            self.draining = true;
            Ok(())
        } else {
            self.disconnect();
            Err(err)
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
            self.disconnect();
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
        self.disconnect();
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
        let Some(current) = self.output() else {
            return Poll::Ready(Ok(buf.len()));
        };
        match ready!(Pin::new(current).poll_write(cx, buf)) {
            Ok(n) => Poll::Ready(Ok(n)),
            Err(err) => Poll::Ready(self.handle_write_error(err).map(|()| buf.len())),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Some(current) = self.output() else {
            return Poll::Ready(Ok(()));
        };
        match ready!(Pin::new(current).poll_flush(cx)) {
            Ok(()) => Poll::Ready(Ok(())),
            Err(err) => Poll::Ready(self.handle_write_error(err)),
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Some(current) = self.output() else {
            return Poll::Ready(Ok(()));
        };
        match ready!(Pin::new(current).poll_close(cx)) {
            Ok(()) => Poll::Ready(Ok(())),
            Err(err) => Poll::Ready(self.handle_write_error(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::AsyncReadExt;
    use futures::AsyncWriteExt;
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

    /// Sends `input` from `peer` and waits until it is queued on the backend's
    /// socket.
    async fn send_input(backend: &mut SocketSerialBackend, peer: &mut TcpStream, input: &[u8]) {
        peer.write_all(input).unwrap();
        let current = backend.current.as_mut().unwrap();
        assert_eq!(
            poll_fn(|cx| current.poll_peek(cx, &mut [0])).await.unwrap(),
            1
        );
    }

    fn reset_peer(peer: TcpStream) {
        let peer = Socket::from(peer);
        peer.set_linger(Some(Duration::ZERO)).unwrap();
        drop(peer);
    }

    /// Writes until the backend notices that the reset peer is gone. Returns
    /// the error if the backend reported one, or `Ok` if it kept the
    /// connection to drain queued input instead.
    async fn write_until_peer_gone(backend: &mut SocketSerialBackend) -> io::Result<()> {
        let buf = [0x5a; 64 * 1024];
        for _ in 0..128 {
            assert_ne!(backend.write(&buf).await?, 0);
            if backend.draining {
                return Ok(());
            }
            std::thread::yield_now();
        }
        panic!("reset peer did not fail a write after bounded retries");
    }

    /// Checks that the backend discards output while it delivers `input`.
    async fn assert_draining(backend: &mut SocketSerialBackend, input: &[u8]) {
        assert!(backend.draining);
        assert!(backend.is_connected());
        assert_eq!(backend.write(b"dropped").await.unwrap(), 7);
        let mut buf = vec![0; input.len()];
        backend.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, input);
    }

    #[async_test]
    async fn write_failure_drops_connection_and_accepts_replacement(driver: DefaultDriver) {
        let (mut backend, address) = new_backend(driver);
        let peer = connect_peer(&mut backend, address).await;
        reset_peer(peer);

        let err = write_until_peer_gone(&mut backend).await.unwrap_err();
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::ConnectionReset
            ),
            "unexpected reset error: {err:?}"
        );
        assert!(!backend.is_connected());
        assert_eq!(backend.write(b"dropped").await.unwrap(), 7);

        let _replacement = connect_peer(&mut backend, address).await;
    }

    // Linux keeps input that arrived before a reset readable. Windows discards
    // it when the reset arrives, so there is nothing to drain there.
    #[cfg(target_os = "linux")]
    #[async_test]
    async fn write_failure_drains_input_queued_before_reset(driver: DefaultDriver) {
        let (mut backend, address) = new_backend(driver);
        let mut peer = connect_peer(&mut backend, address).await;
        send_input(&mut backend, &mut peer, b"input").await;
        reset_peer(peer);

        write_until_peer_gone(&mut backend).await.unwrap();
        assert_draining(&mut backend, b"input").await;
        assert_eq!(backend.read(&mut [0]).await.unwrap(), 0);
        assert!(!backend.is_connected());

        let _replacement = connect_peer(&mut backend, address).await;
    }

    #[async_test]
    async fn write_failure_drains_input_until_peer_disconnects(driver: DefaultDriver) {
        let (mut backend, address) = new_backend(driver);
        let mut peer = connect_peer(&mut backend, address).await;
        send_input(&mut backend, &mut peer, b"input").await;

        // Shut down the send side so that the next write fails while the
        // peer's input is still queued.
        backend.close().await.unwrap();
        assert_eq!(backend.write(b"output").await.unwrap(), 6);
        assert_draining(&mut backend, b"input").await;
        drop(peer);
        assert_eq!(backend.read(&mut [0]).await.unwrap(), 0);
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
