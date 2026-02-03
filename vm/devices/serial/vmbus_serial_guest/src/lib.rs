// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements a UART backend that communicates with the host over a VMBUS pipe.

#![forbid(unsafe_code)]

pub use vmbus_serial_protocol::UART_INTERFACE_INSTANCE_COM1;
pub use vmbus_serial_protocol::UART_INTERFACE_INSTANCE_COM2;
pub use vmbus_serial_protocol::UART_INTERFACE_INSTANCE_COM3;
pub use vmbus_serial_protocol::UART_INTERFACE_INSTANCE_COM4;

use futures::AsyncRead;
use futures::AsyncWrite;
use inspect::Inspect;
use inspect::InspectMut;
use inspect_counters::Counter;
use mesh::MeshPayload;
use protocol::GuestNotifications;
use protocol::HostRequests;
use protocol::MAX_MESSAGE_SIZE;
use protocol::MessageTypes;
use protocol::MessageVersions;
use protocol::UART_MSG_MAX_PAYLOAD;
use serial_core::SerialIo;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::fs::File;
use std::future::poll_fn;
use std::io;
use std::io::IoSlice;
use std::io::IoSliceMut;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::task::ready;
use thiserror::Error;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::SerialBackendHandle;
use vmbus_async::async_dgram::AsyncRecv;
use vmbus_async::async_dgram::AsyncRecvExt;
use vmbus_async::async_dgram::AsyncSend;
use vmbus_async::async_dgram::AsyncSendExt;
use vmbus_serial_protocol as protocol;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

/// Configuration for an open vmbus serial port resource.
#[derive(MeshPayload)]
pub struct OpenVmbusSerialGuestConfig {
    /// The open UIO device file.
    pub uio_device: File,
    /// Only allow guest to host traffic
    pub tx_only: bool,
}

impl ResourceId<SerialBackendHandle> for OpenVmbusSerialGuestConfig {
    const ID: &'static str = "vmbus";
}

/// Resolver for vmbus serial port resources.
pub struct VmbusSerialGuestResolver;

#[cfg(target_os = "linux")]
mod user_pipe {
    use crate::OpenVmbusSerialGuestConfig;
    use crate::VmbusSerialDriver;
    use crate::VmbusSerialGuestResolver;
    use anyhow::Context;
    use async_trait::async_trait;
    use guid::Guid;
    use serial_core::resources::ResolveSerialBackendParams;
    use serial_core::resources::ResolvedSerialBackend;
    use vm_resource::AsyncResolveResource;
    use vm_resource::ResourceResolver;
    use vm_resource::declare_static_async_resolver;
    use vm_resource::kind::SerialBackendHandle;

    impl OpenVmbusSerialGuestConfig {
        /// Opens the UIO device for the specified instance GUID and returns the
        /// configuration resource.
        pub fn open(instance_id: &Guid, tx_only: bool) -> anyhow::Result<Self> {
            let uio_device = vmbus_user_channel::open_uio_device(instance_id)?;
            Ok(Self {
                uio_device,
                tx_only,
            })
        }
    }

    declare_static_async_resolver!(
        VmbusSerialGuestResolver,
        (SerialBackendHandle, OpenVmbusSerialGuestConfig)
    );

    #[async_trait]
    impl AsyncResolveResource<SerialBackendHandle, OpenVmbusSerialGuestConfig>
        for VmbusSerialGuestResolver
    {
        type Output = ResolvedSerialBackend;
        type Error = anyhow::Error;

        async fn resolve(
            &self,
            _resolver: &ResourceResolver,
            rsrc: OpenVmbusSerialGuestConfig,
            input: ResolveSerialBackendParams<'_>,
        ) -> Result<Self::Output, Self::Error> {
            let pipe = vmbus_user_channel::message_pipe(input.driver.as_ref(), rsrc.uio_device)
                .context("failed to open vmbus serial")?;

            let driver = VmbusSerialDriver::new(pipe, rsrc.tx_only)
                .await
                .context("failed to create serial transport")?;

            Ok(driver.into())
        }
    }
}

/// A connected instance of a vmbus serial port.
#[derive(InspectMut)]
pub struct VmbusSerialDriver {
    #[inspect(mut)]
    pipe: Box<dyn Pipe>,
    #[inspect(with = "VecDeque::len")]
    rx_buffer: VecDeque<u8>,
    tx_in_flight: bool,
    rx_in_flight: bool,
    rx_avail: bool,
    #[inspect(with = "Option::is_some")]
    rx_waker: Option<Waker>,
    #[inspect(with = "Option::is_some")]
    tx_waker: Option<Waker>,
    failed: bool,
    connected: bool,
    stats: SerialStats,
    tx_only: bool,
}

#[derive(Inspect, Debug, Default)]
struct SerialStats {
    rx_bytes: Counter,
    tx_bytes: Counter,
}

trait Pipe: AsyncRecv + AsyncSend + Send + InspectMut + Unpin {}

impl<T: AsyncRecv + AsyncSend + Send + InspectMut + Unpin> Pipe for T {}

/// A pipe error.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] ErrorInner);

#[derive(Debug, Error)]
enum ErrorInner {
    #[error("i/o error")]
    Io(#[from] io::Error),
    #[error("truncated message")]
    TruncatedMessage,
    #[error("invalid message type {0:?}")]
    InvalidMessageType(MessageTypes),
    #[error("invalid host response {0:?}")]
    InvalidHostResponse(HostRequests),
    #[error("invalid guest notification {0:?}")]
    InvalidGuestNotification(GuestNotifications),
    #[error("invalid message version {0:?}")]
    InvalidMessageVersion(MessageVersions),
    #[error("invalid buffer length in message")]
    InvalidBufferLength,
    #[error("version not accepted")]
    VersionNotAccepted,
    #[error("failed device")]
    FailedDevice,
}

impl From<ErrorInner> for io::Error {
    fn from(value: ErrorInner) -> Self {
        Self::other(value)
    }
}

impl From<VmbusSerialDriver> for Resource<SerialBackendHandle> {
    fn from(_value: VmbusSerialDriver) -> Self {
        unimplemented!("underhill does not yet rely on this path")
    }
}

impl VmbusSerialDriver {
    /// Connects to `pipe` and returns a new serial device instance.
    pub async fn new(
        pipe: impl 'static + AsyncRecv + AsyncSend + Send + Unpin + InspectMut,
        tx_only: bool,
    ) -> Result<Self, Error> {
        let mut this = Self {
            pipe: Box::new(pipe),
            rx_buffer: VecDeque::new(),
            tx_in_flight: false,
            rx_in_flight: false,
            rx_avail: false,
            rx_waker: None,
            tx_waker: None,
            failed: false,
            connected: false,
            stats: Default::default(),
            tx_only,
        };
        this.negotiate().await?;
        Ok(this)
    }

    /// Wait for any pending rx to be received.
    pub async fn drain_rx(&mut self) -> Result<(), Error> {
        poll_fn(|cx| {
            while self.rx_in_flight {
                ready!(self.poll_outer(cx))?;
            }
            Poll::Ready(Ok(()))
        })
        .await
    }

    async fn negotiate(&mut self) -> Result<(), ErrorInner> {
        let request = protocol::VersionRequestMessage {
            header: protocol::Header::new_host_request(HostRequests::VERSION),
            requested_version: protocol::ProtocolVersions::MANGANESE,
        };

        self.pipe.as_mut().send(request.as_bytes()).await?;

        let mut buf = [0; MAX_MESSAGE_SIZE];
        let n = self.pipe.as_mut().recv(&mut buf).await?;
        let response = protocol::VersionRequestResponse::read_from_prefix(&buf[..n])
            .map_err(|_| ErrorInner::TruncatedMessage)?
            .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

        let host_response = response
            .header
            .host_response()
            .ok_or(ErrorInner::InvalidMessageType(response.header.message_type))?;

        if host_response != HostRequests::VERSION {
            return Err(ErrorInner::InvalidHostResponse(host_response));
        }
        if response.header.message_version != MessageVersions::HEADER_VERSION_1 {
            return Err(ErrorInner::InvalidMessageVersion(
                response.header.message_version,
            ));
        }
        if response.version_accepted == 0 {
            return Err(ErrorInner::VersionNotAccepted);
        }

        // Poll once to get modem status, which the host should send right away.
        poll_fn(|cx| self.poll_outer(cx)).await?;

        Ok(())
    }

    fn poll_outer(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), ErrorInner>> {
        if self.failed {
            Poll::Ready(Err(ErrorInner::FailedDevice))
        } else {
            let r = self.poll_inner(cx);
            if let Poll::Ready(Err(err)) = &r {
                tracing::error!(
                    error = err as &dyn std::error::Error,
                    "serial device failure"
                );
                self.failed = true;
                self.connected = false;
            }
            r
        }
    }

    fn poll_inner(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), ErrorInner>> {
        // Start a new RX if possible.
        if self.rx_buffer.is_empty()
            && self.rx_waker.is_some()
            && self.rx_avail
            && !self.rx_in_flight
        {
            assert!(!self.tx_only);

            let request = protocol::Header::new_host_request(HostRequests::GET_RX_DATA);
            if let Poll::Ready(r) =
                Pin::new(self.pipe.as_mut()).poll_send(cx, &[IoSlice::new(request.as_bytes())])
            {
                r?;
                self.rx_in_flight = true;
                self.rx_avail = false;
            }
        }

        // Poll for messages until there are no more to handle. Return
        // `Poll::Ready` if at least one message was handled.
        //
        // This is needed to ensure that we continue to poll for work for the rx
        // path even if the tx path succeeds, or vice versa.
        let mut buf = [0; MAX_MESSAGE_SIZE];
        let mut result = Poll::Pending;
        while let Poll::Ready(n) =
            Pin::new(self.pipe.as_mut()).poll_recv(cx, &mut [IoSliceMut::new(&mut buf)])?
        {
            self.handle_message(&buf[..n])?;
            result = Poll::Ready(Ok(()));
        }
        result
    }

    fn handle_message(&mut self, buf: &[u8]) -> Result<(), ErrorInner> {
        let header = protocol::Header::read_from_prefix(buf)
            .map_err(|_| ErrorInner::TruncatedMessage)?
            .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
        if header.message_version != MessageVersions::HEADER_VERSION_1 {
            return Err(ErrorInner::InvalidMessageVersion(header.message_version));
        }
        if let Some(req) = header.host_response() {
            match req {
                HostRequests::GET_RX_DATA => {
                    assert!(!self.tx_only);

                    let response = protocol::RxDataResponse::read_from_prefix(buf)
                        .map_err(|_| ErrorInner::TruncatedMessage)?
                        .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

                    let b = response
                        .buffer
                        .get(..response.buffer_length as usize)
                        .ok_or(ErrorInner::InvalidBufferLength)?;

                    self.rx_in_flight = false;
                    self.rx_avail = response.more_data_available != 0;
                    self.rx_buffer.extend(b);
                    self.stats.rx_bytes.add(b.len() as u64);
                    if let Some(waker) = self.rx_waker.take() {
                        waker.wake();
                    }
                }
                req => {
                    return Err(ErrorInner::InvalidHostResponse(req));
                }
            }
        } else if let Some(notif) = header.guest_notification() {
            match notif {
                GuestNotifications::RX_DATA_AVAILABLE => {
                    // This notification is ignored in TX only mode.
                    if !self.tx_only {
                        self.rx_avail = true
                    }
                }
                GuestNotifications::SET_MODEM_STATUS => {
                    let status = protocol::SetModumStatusMessage::read_from_prefix(buf)
                        .map_err(|_| ErrorInner::TruncatedMessage)?
                        .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

                    self.connected = status.is_connected != 0;
                }
                GuestNotifications::TX_COMPLETED => {
                    assert!(self.tx_in_flight);
                    self.tx_in_flight = false;
                    if let Some(waker) = self.tx_waker.take() {
                        waker.wake();
                    }
                }
                notif => return Err(ErrorInner::InvalidGuestNotification(notif)),
            }
        } else {
            return Err(ErrorInner::InvalidMessageType(header.message_type));
        }
        Ok(())
    }
}

impl SerialIo for VmbusSerialDriver {
    fn is_connected(&self) -> bool {
        self.connected
    }

    fn poll_connect(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while !self.connected {
            ready!(self.poll_outer(cx))?;
        }
        Poll::Ready(Ok(()))
    }

    fn poll_disconnect(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.connected {
            ready!(self.poll_outer(cx))?;
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for VmbusSerialDriver {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        while self.rx_buffer.is_empty() || self.failed {
            if !self.connected && !self.failed {
                return Poll::Ready(Ok(0));
            }
            self.rx_waker = Some(cx.waker().clone());
            ready!(self.poll_outer(cx))?;
        }

        assert!(!self.tx_only);

        let n = buf.len().min(self.rx_buffer.len());
        for (s, d) in self.rx_buffer.drain(..n).zip(buf) {
            *d = s;
        }
        Poll::Ready(Ok(n))
    }
}

impl AsyncWrite for VmbusSerialDriver {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        while self.tx_in_flight || self.failed {
            self.tx_waker = Some(cx.waker().clone());
            ready!(self.poll_outer(cx))?;
        }
        let buf = &buf[..buf.len().min(UART_MSG_MAX_PAYLOAD)];
        let mut request = protocol::TxDataAvailableMessage {
            header: protocol::Header::new_host_notification(
                protocol::HostNotifications::TX_DATA_AVAILABLE,
            ),
            buffer_length: buf.len() as u8,
            buffer: [0; UART_MSG_MAX_PAYLOAD],
            pad: 0,
        };
        request.buffer[..buf.len()].copy_from_slice(buf);
        std::task::ready!(
            Pin::new(self.pipe.as_mut()).poll_send(cx, &[IoSlice::new(request.as_bytes())])
        )?;
        self.tx_in_flight = true;
        self.stats.tx_bytes.add(buf.len() as u64);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.tx_in_flight || self.failed {
            ready!(self.poll_outer(cx))?;
        }
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use crate::ErrorInner;
    use crate::VmbusSerialDriver;
    use futures::AsyncWriteExt;
    use futures::FutureExt;
    use futures::io::AsyncReadExt;
    use futures::join;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::socket::PolledSocket;
    use pal_async::task::Spawn;
    use serial_core::serial_io::Connected;
    use test_with_tracing::test;
    use unix_socket::UnixStream;
    use vmbus_async::async_dgram::AsyncRecvExt;
    use vmbus_async::async_dgram::AsyncSendExt;
    use vmbus_serial_host::Serial;
    use vmbus_serial_host::SerialChannel;
    use vmbus_serial_protocol::*;
    use zerocopy::IntoBytes;

    #[async_test]
    async fn test_version_negotiation_failed(driver: DefaultDriver) {
        let (mut host_vmbus, guest_vmbus) = vmbus_async::pipe::connected_message_pipes(4096);

        let host_task = driver.spawn("test", async move {
            let mut version_request = VersionRequestMessage::default();
            let len = host_vmbus
                .recv(version_request.as_mut_bytes())
                .await
                .unwrap();

            assert_eq!(len, version_request.as_bytes().len());
            assert_eq!(
                version_request.header,
                Header::new_host_request(HostRequests::VERSION)
            );
            assert_eq!(
                version_request.requested_version,
                ProtocolVersions::MANGANESE
            );

            // Reject the request.
            let version_response = VersionRequestResponse {
                header: Header::new_host_response(HostRequests::VERSION),
                version_accepted: 0,
                pad: 0,
            };

            host_vmbus.send(version_response.as_bytes()).await.unwrap();
        });

        let res = VmbusSerialDriver::new(guest_vmbus, false).await;
        match res {
            Err(crate::Error(ErrorInner::VersionNotAccepted)) => {}
            Err(e) => panic!("Wrong error type returned {e:?}"),
            Ok(_) => panic!("Expected failure, got success"),
        }

        host_task.await;
    }

    /// Creates a new host guest transport pair ready to send data.
    async fn new_transport_pair(
        driver: &DefaultDriver,
        tx_only: bool,
    ) -> (PolledSocket<UnixStream>, VmbusSerialDriver) {
        let (host_vmbus, guest_vmbus) = vmbus_async::pipe::connected_message_pipes(4096);

        let (host_io, guest_io) = UnixStream::pair().unwrap();
        let host_io = PolledSocket::new(driver, host_io).unwrap();
        let guest_io = PolledSocket::new(driver, guest_io).unwrap();

        // Create the host serial channel and corresponding io
        let mut serial = Serial::new(
            vmbus_serial_host::Port::Com1,
            Box::new(Connected::new(guest_io)),
        );

        let mut host_serial_channel = SerialChannel::new(host_vmbus);
        driver
            .spawn("vmbus host serial", async move {
                host_serial_channel.test_process(&mut serial).await;
            })
            .detach();

        // Create the guest serial transport
        let guest_transport = VmbusSerialDriver::new(guest_vmbus, tx_only).await.unwrap();

        (host_io, guest_transport)
    }

    #[async_test]
    async fn test_basic_read_write(driver: DefaultDriver) {
        let (mut host_io, mut guest_io) = new_transport_pair(&driver, false).await;

        let data = vec![1, 2, 3, 4, 5];
        let data2 = vec![5, 4, 3, 2, 1];
        host_io.write_all(&data).await.unwrap();
        guest_io.write_all(&data2).await.unwrap();
        let mut data_recv = vec![0; 5];
        let mut data_recv2 = vec![0; 5];
        guest_io.read_exact(&mut data_recv).await.unwrap();
        host_io.read_exact(&mut data_recv2).await.unwrap();
        assert_eq!(data, data_recv);
        assert_eq!(data2, data_recv2);
    }

    #[async_test]
    async fn test_large_read_write(driver: DefaultDriver) {
        let (host_io, guest_io) = new_transport_pair(&driver, false).await;

        let (mut host_read, mut host_write) = host_io.split();
        let (mut guest_read, mut guest_write) = guest_io.split();

        let data1: Vec<u8> = (0..4096).map(|x| (x % 67) as u8).collect();
        let data1_clone = data1.clone();
        let data2: Vec<u8> = (0..4096).map(|x| (x % 38) as u8).collect();
        let data2_clone = data2.clone();
        let host_write = async {
            host_write.write_all(&data1).await.unwrap();
            host_write.write_all(&data1).await.unwrap();
        };
        let guest_write = async {
            guest_write.write_all(&data2).await.unwrap();
            guest_write.write_all(&data2).await.unwrap();
        };

        let test = async {
            // test read exact full buffer
            let mut recv = vec![0; 4096];
            guest_read.read_exact(&mut recv).await.unwrap();
            assert_eq!(recv, data1_clone);

            host_read.read_exact(&mut recv).await.unwrap();
            assert_eq!(recv, data2_clone);

            // test reading 128 byte chunks at a time
            let mut recv = vec![0; 128];
            for i in 0..32 {
                let start = i * 128;
                let end = (i + 1) * 128;
                guest_read.read_exact(&mut recv).await.unwrap();
                assert_eq!(recv, data1_clone[start..end]);

                host_read.read_exact(&mut recv).await.unwrap();
                assert_eq!(recv, data2_clone[start..end]);
            }
        };

        join!(host_write, guest_write, test);
    }

    #[async_test]
    async fn test_large_duplex_concurrent_io(driver: DefaultDriver) {
        let (host_io, guest_io) = new_transport_pair(&driver, false).await;

        let (mut host_read, mut host_write) = host_io.split();
        let (mut guest_read, mut guest_write) = guest_io.split();

        let data1: Vec<u8> = (0..4096).map(|x| (x % 67) as u8).collect();
        let data1_result = data1.repeat(4);
        let data2: Vec<u8> = (0..4096).map(|x| (x % 38) as u8).collect();
        let data2_result = data2.repeat(4);

        let host_write = async {
            host_write.write_all(&data1).await.unwrap();
            tracing::error!("t1 w1");
            host_write.write_all(&data1).await.unwrap();
            tracing::error!("t1 w2");
            host_write.write_all(&data1).await.unwrap();
            tracing::error!("t1 w3");
            host_write.write_all(&data1).await.unwrap();
            tracing::error!("t1 w4");
            host_write
        };

        let guest_write = async {
            guest_write.write_all(&data2).await.unwrap();
            tracing::error!("t2 w1");
            guest_write.write_all(&data2).await.unwrap();
            tracing::error!("t2 w2");
            guest_write.write_all(&data2).await.unwrap();
            tracing::error!("t2 w3");
            guest_write.write_all(&data2).await.unwrap();
            tracing::error!("t2 w4");
        };

        let host_read = async {
            let mut buf = vec![0; 4096 * 4];
            tracing::error!("t3 read");
            host_read.read_exact(&mut buf).await.unwrap();
            tracing::error!("t3 finished");
            assert_eq!(buf, data2_result);
            host_read
        };

        let guest_read = async {
            let mut buf = vec![0; 4096 * 4];
            tracing::error!("t4 read");
            guest_read.read_exact(&mut buf).await.unwrap();
            tracing::error!("t4 finished");
            assert_eq!(buf, data1_result);
        };

        join!(host_write, host_read, guest_write, guest_read);
    }

    #[async_test]
    async fn test_read_write_tx_only(driver: DefaultDriver) {
        let (mut host_io, mut guest_io) = new_transport_pair(&driver, true).await;

        let data = vec![1, 2, 3, 4, 5];
        let data2 = vec![5, 4, 3, 2, 1];
        host_io.write_all(&data).await.unwrap();
        guest_io.write_all(&data2).await.unwrap();

        let mut data_recv = vec![0; 5];
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            guest_io.read(&mut data_recv).poll_unpin(&mut cx),
            std::task::Poll::Pending
        ));

        let mut data_recv2 = vec![0; 5];
        host_io.read_exact(&mut data_recv2).await.unwrap();
        assert_eq!(data2, data_recv2);
    }
}
