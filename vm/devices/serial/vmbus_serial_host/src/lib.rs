// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The host (server) side implementation of a VMBUS based serial device.

#![warn(missing_docs)]

pub mod resolver;

use async_trait::async_trait;
use inspect::Inspect;
use inspect::InspectMut;
use inspect_counters::Counter;
use protocol::HostNotifications;
use protocol::HostRequests;
use serial_core::SerialIo;
use std::cmp::min;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::future::poll_fn;
use std::future::Future;
use std::pin::pin;
use std::pin::Pin;
use std::task::ready;
use std::task::Context;
use std::task::Poll;
use task_control::StopTask;
use thiserror::Error;
use vmbus_async::async_dgram::AsyncRecvExt;
use vmbus_async::pipe::MessagePipe;
use vmbus_channel::bus::ChannelType;
use vmbus_channel::bus::OfferParams;
use vmbus_channel::channel::ChannelOpenError;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_channel::simple::SimpleVmbusDevice;
use vmbus_channel::RawAsyncChannel;
use vmbus_ring::RingMem;
use vmbus_serial_protocol as protocol;
use vmcore::save_restore::SavedStateNotSupported;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// The error type returned by the serial device.
#[derive(Debug, Error)]
enum Error {
    #[error("channel i/o error")]
    Io(#[source] std::io::Error),
    #[error("invalid version during version negotiation")]
    InvalidVersionNegotiation,
    #[error("invalid header message version")]
    InvalidMessageHeaderVersion,
    #[error("received out of order packet")]
    UnexpectedPacketOrder,
    #[error("tx is already in flight")]
    TxInFlight,
    #[error("message size of {0} too small to read header")]
    MessageSizeHeader(usize),
    #[error("message size of {len} too small to read host notification {notification:?}")]
    MessageSizeHostNotification {
        len: usize,
        notification: HostNotifications,
    },
    #[error("tx data length of {0} invalid, greater than the maximum")]
    TxDataLength(u8),
    #[error("unknown host notification {notification:?}")]
    UnknownHostNotification { notification: HostNotifications },
    #[error("unknown host request {request:?}")]
    UnknownHostRequest { request: HostRequests },
    #[error("bad message type {0}")]
    BadMessageType(u8),
}

/// Host VMBUS serial device.
#[derive(InspectMut)]
pub struct Serial {
    #[inspect(skip)]
    port: Port,
    #[inspect(mut)]
    io: Box<dyn SerialIo>,
    connected: bool,
    stats: SerialStats,
}

#[derive(Debug, Inspect, Default)]
struct SerialStats {
    rx_bytes: Counter,
    tx_bytes: Counter,
    tx_dropped: Counter,
}

impl Serial {
    /// Create a new Vmbus serial device for the given port.
    pub fn new(port: Port, io: Box<dyn SerialIo>) -> Self {
        Self {
            port,
            connected: io.is_connected(),
            io,
            stats: Default::default(),
        }
    }
}

/// Enum describing VMBUS serial port to offer to the guest.
#[derive(Debug)]
pub enum Port {
    /// COM1 instance id.
    Com1,
    /// COM2 instance id.
    Com2,
}

#[async_trait]
impl SimpleVmbusDevice for Serial {
    type Runner = SerialChannel;
    type SavedState = SavedStateNotSupported;

    fn offer(&self) -> OfferParams {
        let (interface_name, instance_id) = match self.port {
            Port::Com1 => ("serial_com1".into(), protocol::UART_INTERFACE_INSTANCE_COM1),
            Port::Com2 => ("serial_com2".into(), protocol::UART_INTERFACE_INSTANCE_COM2),
        };

        OfferParams {
            interface_name,
            interface_id: protocol::UART_INTERFACE_TYPE,
            instance_id,
            channel_type: ChannelType::Pipe { message_mode: true },
            ..Default::default()
        }
    }

    fn inspect(&mut self, req: inspect::Request<'_>, mut channel: Option<&mut SerialChannel>) {
        req.respond().merge(self).field_mut("channel", &mut channel);
    }

    fn open(
        &mut self,
        channel: RawAsyncChannel<GpadlRingMem>,
        _guest_memory: guestmem::GuestMemory,
    ) -> Result<Self::Runner, ChannelOpenError> {
        let pipe = MessagePipe::new(channel)?;
        Ok(SerialChannel::new(pipe))
    }

    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        channel: &mut SerialChannel,
    ) -> Result<(), task_control::Cancelled> {
        stop.until_stopped(async {
            match channel.process(self).await {
                Ok(()) => {}
                Err(err) => tracing::error!(error = &err as &dyn std::error::Error, "serial error"),
            }
        })
        .await
    }

    fn supports_save_restore(
        &mut self,
    ) -> Option<
        &mut dyn vmbus_channel::simple::SaveRestoreSimpleVmbusDevice<
            SavedState = Self::SavedState,
            Runner = Self::Runner,
        >,
    > {
        None
    }
}

#[derive(Debug, Default, Inspect)]
struct SerialChannelState {
    /// TX bytes from Guest -> Host.
    #[inspect(with = "VecDeque::len")]
    tx_bytes: VecDeque<u8>,
    tx_pending: bool,
    /// RX bytes from Host -> Guest
    #[inspect(with = "VecDeque::len")]
    rx_bytes: VecDeque<u8>,
    send_rx_notification: bool,
    pending_modem_status: bool,
}

impl SerialChannelState {
    const RX_CACHE_SIZE: usize = 1024;
}

/// Serial protocol handling that communicates with the guest with the [`vmbus_serial_protocol`].
#[derive(InspectMut)]
pub struct SerialChannel<T: RingMem = GpadlRingMem> {
    #[inspect(mut)]
    channel: MessagePipe<T>,
    state: SerialChannelState,
    protocol: ProtocolState,
}

#[derive(Inspect)]
enum ProtocolState {
    Init,
    Ready,
}

impl<T: RingMem + Unpin> SerialChannel<T> {
    /// Creates a new serial channel that communicates over the given `channel` with `state`.
    #[doc(hidden)] // This is `pub` for `vmbus_serial_guest`'s tests.
    pub fn new(channel: MessagePipe<T>) -> Self {
        Self {
            channel,
            state: SerialChannelState::default(),
            protocol: ProtocolState::Init,
        }
    }

    #[doc(hidden)] // `pub` for `vmbus_serial_guest`'s tests.
    pub async fn test_process(&mut self, serial: &mut Serial) {
        self.process(serial).await.unwrap()
    }

    /// Drive the protocol processing loop to communicate with the guest. This function returns `Ok(())` if the
    /// associated sender for `state_changed_notification` was dropped.
    async fn process(&mut self, serial: &mut Serial) -> Result<(), Error> {
        loop {
            // Wait for enough space for a response message before processing incoming messages.
            self.channel
                .wait_write_ready(protocol::MAX_MESSAGE_SIZE)
                .await
                .map_err(Error::Io)?;

            match self.protocol {
                ProtocolState::Init => self.process_init().await?,
                ProtocolState::Ready => {
                    if !self.process_ready(serial).await? {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    async fn process_init(&mut self) -> Result<(), Error> {
        // Negotiate transport version with the client.
        let mut version_request = protocol::VersionRequestMessage::default();
        self.read_pipe(version_request.as_mut_bytes()).await?;
        tracing::trace!(?version_request);

        if version_request.header != protocol::Header::new_host_request(HostRequests::VERSION) {
            tracing::trace!("invalid first packet type");
            return Err(Error::UnexpectedPacketOrder);
        }

        if version_request.requested_version != protocol::ProtocolVersions::MANGANESE {
            tracing::trace!(version = ?version_request.requested_version, "invalid version request");
            return Err(Error::InvalidVersionNegotiation);
        }

        let version_response = protocol::VersionRequestResponse {
            header: protocol::Header::new_host_response(HostRequests::VERSION),
            version_accepted: 1,
            pad: 0,
        };

        self.write_pipe(version_response)?;
        self.protocol = ProtocolState::Ready;

        // Send modem status right away so that the guest knows the connection state.
        self.state.pending_modem_status = true;
        Ok(())
    }

    async fn process_ready(&mut self, serial: &mut Serial) -> Result<bool, Error> {
        let mut buf = [0; protocol::MAX_MESSAGE_SIZE];

        #[derive(Debug)]
        enum Event {
            Packet(Result<usize, std::io::Error>),
            SendModemStatus,
            SendTxCompletion,
            SendRxAvailable,
        }

        let event = {
            let mut read_header = pin!(async { self.channel.recv(&mut buf).await });
            poll_fn(|cx| {
                // Check for tx.
                let _ = self.state.poll_tx(cx, serial);
                // Check for rx.
                let _ = self.state.poll_rx(cx, serial);

                // Break out to send notifications.
                if self.state.tx_pending && self.state.tx_bytes.is_empty() {
                    Poll::Ready(Event::SendTxCompletion)
                } else if self.state.send_rx_notification {
                    Poll::Ready(Event::SendRxAvailable)
                } else if self.state.pending_modem_status {
                    Poll::Ready(Event::SendModemStatus)
                } else {
                    // Read the next packet.
                    read_header.as_mut().poll(cx).map(Event::Packet)
                }
            })
            .await
        };

        match event {
            Event::Packet(result) => {
                let n = match result {
                    Ok(n) => n,
                    Err(err) if err.kind() == std::io::ErrorKind::ConnectionReset => {
                        // The other side has closed the channel.
                        tracing::trace!("serial channel closed, returning");
                        return Ok(false);
                    }
                    Err(err) => return Err(Error::Io(err)),
                };
                self.process_header(serial, &buf[..n])?;
            }
            Event::SendModemStatus => {
                // Set 16550 CTS, DSR, DCD, and matching change registers.
                // Underhill ignores this field, but set it for completelness.
                let modem_status = if serial.connected { 0xbb } else { 0x0b };
                let is_connected = serial.connected.into();

                let message = protocol::SetModumStatusMessage {
                    header: protocol::Header::new_guest_notification(
                        protocol::GuestNotifications::SET_MODEM_STATUS,
                    ),
                    is_connected,
                    modem_status,
                };
                self.write_pipe(message)?;
                self.state.pending_modem_status = false;
            }
            Event::SendTxCompletion => {
                self.write_pipe(protocol::Header::new_guest_notification(
                    protocol::GuestNotifications::TX_COMPLETED,
                ))?;
                self.state.tx_pending = false;
            }
            Event::SendRxAvailable => {
                self.write_pipe(protocol::Header::new_guest_notification(
                    protocol::GuestNotifications::RX_DATA_AVAILABLE,
                ))?;
                self.state.send_rx_notification = false;
            }
        }

        Ok(true)
    }

    /// Writes to the pipe. The caller must guarantee that there is enough space.
    fn write_pipe(
        &mut self,
        message: impl IntoBytes + Immutable + KnownLayout,
    ) -> Result<(), Error> {
        self.channel
            .try_send(message.as_bytes())
            .map_err(Error::Io)?;

        Ok(())
    }

    async fn read_pipe(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        self.channel.recv(buf).await.map_err(Error::Io)
    }

    fn process_header(&mut self, serial: &mut Serial, buf: &[u8]) -> Result<(), Error> {
        tracing::trace!(len = buf.len(), "read message len");

        // Extract header from read buf.
        let header = protocol::Header::read_from_prefix(buf)
            .map_err(|_| Error::MessageSizeHeader(buf.len()))?
            .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

        tracing::trace!("read header {:?}", &header);

        // Version must be latest. Stop the server if we receive an invalid one.
        if header.message_version != protocol::MessageVersions::HEADER_VERSION_1 {
            tracing::trace!("invalid header version");
            return Err(Error::InvalidMessageHeaderVersion);
        }

        use vmbus_serial_protocol::MessageTypes;

        match header.message_type {
            MessageTypes::HOST_NOTIFICATION => self.handle_host_notification(
                serial,
                header
                    .host_notification()
                    .expect("should be host notification"),
                buf,
            ),
            MessageTypes::HOST_REQUEST => {
                self.handle_host_request(header.host_request().expect("should be host request"))
            }
            _ => {
                tracing::trace!(message_type = ?header.message_type, "invalid message type");
                Err(Error::BadMessageType(header.message_type.0))
            }
        }
    }

    fn handle_host_notification(
        &mut self,
        serial: &mut Serial,
        notification: HostNotifications,
        buf: &[u8],
    ) -> Result<(), Error> {
        match notification {
            HostNotifications::RX_CLEAR_BUFFER => {
                todo!("clear rx buffer unimplemented")
            }
            HostNotifications::TX_DATA_AVAILABLE => {
                let message = protocol::TxDataAvailableMessage::read_from_prefix(buf)
                    .map_err(|_| {
                        // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                        Error::MessageSizeHostNotification {
                            len: buf.len(),
                            notification,
                        }
                    })?
                    .0;

                if self.state.tx_pending {
                    return Err(Error::TxInFlight);
                }

                self.state.tx_pending = true;
                let buffer = message
                    .buffer
                    .get(..message.buffer_length as usize)
                    .ok_or(Error::TxDataLength(message.buffer_length))?;

                serial.stats.tx_bytes.add(buffer.len() as u64);
                self.state.tx_bytes.extend(buffer);
            }
            notification => {
                tracing::error!(?notification, packet = ?buf, "unknown host notification received");
                return Err(Error::UnknownHostNotification { notification });
            }
        }
        Ok(())
    }

    fn handle_host_request(&mut self, request: HostRequests) -> Result<(), Error> {
        match request {
            HostRequests::GET_RX_DATA => {
                let mut data = Vec::new();
                let length = min(protocol::UART_MSG_MAX_PAYLOAD, self.state.rx_bytes.len());
                data.extend(self.state.rx_bytes.drain(..length));
                data.resize(protocol::UART_MSG_MAX_PAYLOAD, 0);

                let more_data_available = if !self.state.rx_bytes.is_empty() {
                    1
                } else {
                    0
                };

                let message = protocol::RxDataResponse {
                    header: protocol::Header::new_host_response(HostRequests::GET_RX_DATA),
                    buffer_length: length as u8,
                    more_data_available,
                    buffer: data.try_into().unwrap(),
                };

                self.write_pipe(message)
            }
            _ => Err(Error::UnknownHostRequest { request }),
        }
    }
}

impl SerialChannelState {
    fn poll_tx(&mut self, cx: &mut Context<'_>, serial: &mut Serial) -> Poll<()> {
        while !self.tx_bytes.is_empty() {
            let (buf, _) = self.tx_bytes.as_slices();
            match ready!(Pin::new(serial.io.as_mut()).poll_write(cx, buf)) {
                Ok(n) => {
                    self.tx_bytes.drain(..n);
                }
                Err(err) => {
                    tracing::error!(
                        len = buf.len(),
                        error = &err as &dyn std::error::Error,
                        "serial write failed, dropping data"
                    );
                    serial.stats.tx_dropped.add(buf.len() as u64);
                    self.tx_bytes.drain(..buf.len());
                }
            }
        }
        Poll::Ready(())
    }

    fn poll_rx(&mut self, cx: &mut Context<'_>, serial: &mut Serial) -> Poll<()> {
        let mut buf = [0; 1024];
        loop {
            if !serial.connected {
                match ready!(serial.io.poll_connect(cx)) {
                    Ok(()) => {
                        tracing::info!("serial connected");
                        serial.connected = true;
                        self.pending_modem_status = true;
                    }
                    Err(err) => {
                        tracing::error!(
                            error = &err as &dyn std::error::Error,
                            "failed to poll serial for connect"
                        );
                        break;
                    }
                }
            }

            let avail_space =
                (SerialChannelState::RX_CACHE_SIZE - self.rx_bytes.len()).min(buf.len());
            if avail_space == 0 {
                if let Err(err) = ready!(serial.io.poll_disconnect(cx)) {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "failed to poll serial for disconnect"
                    );
                    break;
                }
                tracing::info!("serial disconnected");
                serial.connected = false;
                self.pending_modem_status = true;
                continue;
            }
            let buf = &mut buf[..avail_space];
            let n = match ready!(Pin::new(serial.io.as_mut()).poll_read(cx, buf)) {
                Ok(0) => {
                    tracing::info!("serial disconnected");
                    serial.connected = false;
                    self.pending_modem_status = true;
                    continue;
                }
                Ok(n) => n,
                Err(err) => {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "failed to read serial input"
                    );
                    break;
                }
            };
            self.send_rx_notification |= self.rx_bytes.is_empty();
            self.rx_bytes.extend(&buf[..n]);
            serial.stats.rx_bytes.add(n as u64);
        }
        Poll::Ready(())
    }
}
