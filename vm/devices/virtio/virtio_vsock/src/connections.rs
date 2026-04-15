// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Connection manager for the virtio-vsock device.

use crate::LockedIoSliceMut;
use crate::PendingFutures;
use crate::RxReady;
use crate::WriteReadyItem;
use crate::lock_payload_data;
use crate::ring::RingBuffer;
use crate::spec::Operation;
use crate::spec::ShutdownFlags;
use crate::spec::SocketType;
use crate::spec::VSOCK_CID_HOST;
use crate::spec::VSOCK_HEADER_SIZE;
use crate::spec::VsockHeader;
use crate::spec::VsockPacket;
use crate::spec::VsockPacketBuf;
use crate::unix_relay::RelaySocket;
use crate::unix_relay::UnixSocketRelay;
use anyhow::Context;
use bitfield_struct::bitfield;
use guestmem::GuestMemory;
use hybrid_vsock::HYBRID_CONNECT_REQUEST_LEN;
use hybrid_vsock::VsockPortOrId;
use pal_async::timer::Instant;
use pal_async::timer::PolledTimer;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::io::IoSlice;
use std::num::Wrapping;
use std::path::PathBuf;
use std::time::Duration;
use unix_socket::UnixStream;
use virtio::queue::VirtioQueuePayload;
use vmcore::vm_task::VmTaskDriver;

pub const TX_BUF_SIZE: u32 = 65536;
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_ACTIVE_CONNECTIONS: usize = 1024;

/// A key that uniquely identifies a vsock connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionKey {
    local_port: u32,
    peer_port: u32,
}

impl ConnectionKey {
    /// Create a connection key from the header of a packet received from the guest.
    pub fn from_tx_packet(hdr: &VsockHeader) -> Self {
        Self {
            local_port: hdr.dst_port,
            peer_port: hdr.src_port,
        }
    }

    /// Create a connection key from the header of a packet being sent to the guest.
    pub fn from_rx_packet(hdr: &VsockHeader) -> Self {
        Self {
            local_port: hdr.src_port,
            peer_port: hdr.dst_port,
        }
    }
}

/// A connection key combined with a sequence number to distinguish connections when a port is
/// reused after a connection was closed, in case some futures for the old connection may still be
/// pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionInstanceId {
    pub key: ConnectionKey,
    pub seq: u64,
}

/// Indicates which control packets a connection has pending to send to the guest.
#[bitfield(u32)]
struct PendingControlPackets {
    respond: bool,
    credit_request: bool,
    credit_update: bool,
    #[bits(29)]
    _reserved: u32,
}

/// The state of a vsock connection.
#[derive(Debug, PartialEq, Eq)]
enum ConnectionState {
    /// The host initiated the connection, and we're waiting to process its CONNECT request.
    HostConnecting {
        // Buffer for reading the CONNECT request and writing the OK response.
        buffer: Vec<u8>,
        // Number of bytes read or written out of the buffer.
        read_write_count: usize,
        // Whether the CONNECT request used a GUID, or None if we haven't read the request yet.
        use_guid: Option<bool>,
    },
    /// The guest initiated the connection, and we're waiting to send the RESPONSE operation.
    GuestConnecting,
    Connected,
}

/// Tracks the state of a single vsock connection relayed to a Unix socket.
struct Connection {
    key: ConnectionKey,
    seq: u64,
    state: ConnectionState,
    socket: RelaySocket,
    /// Buffer allocation advertised by the peer (guest).
    peer_buf_alloc: u32,
    /// Received data that the peer has forwarded from its buffer.
    peer_fwd_cnt: u32,
    /// Amount of data sent to the peer.
    tx_cnt: Wrapping<u32>,
    /// Data received from the peer that has been forwarded to the unix socket relay.
    fwd_cnt: Wrapping<u32>,
    /// The last value of fwd_cnt that we've told the peer about.
    last_sent_fwd_count: u32,
    /// Control packets waiting to be sent to the guest.
    pending_reply: PendingControlPackets,
    /// A buffer for data that the relay socket couldn't accept. Only allocated when needed.
    recv_buf: Option<RingBuffer>,
    /// Time at which the connection becomes invalid, indicating a connection or a graceful shutdown
    /// timeout.
    timeout: Option<Instant>,
    /// Indicates whether the local port for this connection was allocated and should be freed on
    /// removal.
    allocated_local_port: bool,
    /// Indicates a credit request was sent and we're waiting for a credit update.
    waiting_for_credit: bool,
    /// Indicates the peer has shutdown its write side.
    send_shutdown: bool,
    /// Indicates the peer has shutdown its read side.
    receive_shutdown: bool,
    /// Indicates the unix socket relay has shutdown its write side, so we will no longer send data
    /// to the guest.
    local_send_shutdown: bool,
}

impl Connection {
    /// Create a new guest-initiated connection.
    fn new_guest_initiated(
        key: ConnectionKey,
        seq: u64,
        peer_buf_alloc: u32,
        peer_fwd_cnt: u32,
        pending_reply: PendingControlPackets,
        socket: RelaySocket,
    ) -> Self {
        Self {
            key,
            seq,
            peer_buf_alloc,
            peer_fwd_cnt,
            tx_cnt: Wrapping(0),
            fwd_cnt: Wrapping(0),
            last_sent_fwd_count: 0,
            pending_reply,
            socket,
            recv_buf: None,
            state: ConnectionState::GuestConnecting,
            timeout: None,
            send_shutdown: false,
            receive_shutdown: false,
            local_send_shutdown: false,
            allocated_local_port: false,
            waiting_for_credit: false,
        }
    }

    /// Create a new host-initiated connection.
    fn new_host_initiated(local_port: u32, seq: u64, socket: RelaySocket) -> Self {
        Self {
            key: ConnectionKey {
                local_port,
                // Not known yet.
                peer_port: 0,
            },
            seq,
            // Allocate space for reading the CONNECT request, and sending the OK response.
            state: ConnectionState::HostConnecting {
                buffer: vec![0; HYBRID_CONNECT_REQUEST_LEN],
                read_write_count: 0,
                use_guid: None,
            },
            peer_buf_alloc: 0,
            peer_fwd_cnt: 0,
            tx_cnt: Wrapping(0),
            fwd_cnt: Wrapping(0),
            last_sent_fwd_count: 0,
            pending_reply: PendingControlPackets::new(),
            socket,
            recv_buf: None,
            timeout: Some(Instant::now() + CONNECTION_TIMEOUT),
            send_shutdown: false,
            receive_shutdown: false,
            local_send_shutdown: false,
            allocated_local_port: true,
            waiting_for_credit: false,
        }
    }

    /// Get the unique instance ID of this connection.
    fn instance_id(&self) -> ConnectionInstanceId {
        ConnectionInstanceId {
            key: self.key,
            seq: self.seq,
        }
    }

    /// Handle RW packets from the guest.
    fn handle_guest_data(
        &mut self,
        data: &[IoSlice<'_>],
        data_len: usize,
    ) -> anyhow::Result<Option<WriteReadyItem>> {
        if self.state != ConnectionState::Connected {
            anyhow::bail!("peer sent data before connection established");
        }

        if self.send_shutdown {
            anyhow::bail!("peer has shutdown write side but sent data");
        }

        let bytes_sent = if self.is_recv_buf_empty() {
            // The buffer is empty so try to write the data to the socket.
            self.socket
                .write_vectored(data)
                .context("failed write data to relay socket")?
        } else {
            // There is already buffered data, so that needs to be sent first, and the additional
            // data should be added to the buffer.
            0
        };

        self.fwd_cnt += bytes_sent as u32;
        let remaining = data_len - bytes_sent;
        if remaining == 0 {
            // All data was sent.
            return Ok(None);
        }

        // Not all data was sent, so buffer the remaining data.
        let buf = self
            .recv_buf
            .get_or_insert_with(|| RingBuffer::new(TX_BUF_SIZE as usize));

        // The guest should not do this since it knows how much space we have.
        if remaining > buf.available() {
            anyhow::bail!(
                "peer sent {} bytes, but only {} bytes available in buffer",
                remaining,
                buf.available()
            );
        }

        tracing::debug!(remaining, ring_len = buf.len(), "buffering data from guest");
        buf.write(data, bytes_sent);
        Ok(self.socket.await_write_ready(self.instance_id()))
    }

    /// Write buffered data to the relay socket.
    fn write_from_buffer(&mut self) -> anyhow::Result<Option<WriteReadyItem>> {
        let ring = self.recv_buf.as_mut().expect("buffer must exist");
        let sent = self
            .socket
            .write_from_ring(ring)
            .context("failed to write buffered data to relay socket")?;

        self.fwd_cnt += sent as u32;
        if ring.is_empty() {
            // Check if the peer sent a shutdown while we were waiting to flush data.
            if self.send_shutdown {
                self.socket
                    .shutdown(std::net::Shutdown::Write)
                    .context("failed to shutdown write side of socket")?;
            }

            Ok(None)
        } else {
            // Wait to send the remaining buffer contents.
            Ok(self.socket.await_write_ready(self.instance_id()))
        }
    }

    /// Checks if our fwd count has changed and we need to tell the guest about it.
    fn peer_needs_credit_update(&self) -> bool {
        self.fwd_cnt.0 != self.last_sent_fwd_count
    }

    /// Handle a shutdown request from the guest.
    fn handle_shutdown(&mut self, mut flags: ShutdownFlags) -> anyhow::Result<PendingFutures> {
        if self.state != ConnectionState::Connected {
            anyhow::bail!("peer sent shutdown before connection established");
        }

        if flags.send() {
            // Don't actually shutdown the relay socket if we're still waiting to flush data out of
            // the buffer.
            if !self.is_recv_buf_empty() {
                tracing::debug!("deferring relay shutdown until buffer is flushed");
                flags.set_send(false);
            }

            self.send_shutdown = true;
        }

        let how = if flags.send() {
            if flags.receive() {
                self.receive_shutdown = true;
                Some(std::net::Shutdown::Both)
            } else {
                Some(std::net::Shutdown::Write)
            }
        } else if flags.receive() {
            self.receive_shutdown = true;
            Some(std::net::Shutdown::Read)
        } else {
            None
        };

        if let Some(how) = how {
            self.socket.shutdown(how)?;
        }

        Ok(PendingFutures::NONE)
    }

    /// Return any packet that this connection wants to put on the RX queue.
    fn get_rx_packet(
        &mut self,
        driver: &VmTaskDriver,
        mem: &GuestMemory,
        guest_cid: u64,
        payload: &[VirtioQueuePayload],
    ) -> anyhow::Result<(Option<VsockPacketBuf>, PendingFutures)> {
        if let Some(timeout) = self.timeout {
            if Instant::now() >= timeout {
                anyhow::bail!("connection timed out");
            }
        }

        // First check if there are any pending control packets before checking for data.
        let header = if self.pending_reply.respond() {
            self.pending_reply.set_respond(false);
            self.state = ConnectionState::Connected;
            self.last_sent_fwd_count = self.fwd_cnt.0;

            Some(new_reply_packet(
                self.key,
                Operation::RESPONSE,
                guest_cid,
                self.fwd_cnt.0,
            ))
        } else if self.peer_needs_credit_update() || self.pending_reply.credit_update() {
            let fwd_cnt = self.fwd_cnt.0;
            self.last_sent_fwd_count = fwd_cnt;

            self.pending_reply.set_credit_update(false);
            Some(new_reply_packet(
                self.key,
                Operation::CREDIT_UPDATE,
                guest_cid,
                fwd_cnt,
            ))
        } else if self.pending_reply.credit_request() {
            self.pending_reply.set_credit_request(false);
            Some(new_reply_packet(
                self.key,
                Operation::CREDIT_REQUEST,
                guest_cid,
                self.fwd_cnt.0,
            ))
        } else if self.socket.check_and_clear_has_data() {
            // The relay socket may have data available.
            assert_eq!(self.pending_reply.into_bits(), 0);
            self.handle_host_data(mem, payload, guest_cid)?
        } else {
            // No packets available.
            assert_eq!(self.pending_reply.into_bits(), 0);
            None
        };

        // Check the next thing the worker needs to wait for on this connection.
        let pending_work = if self.pending_reply.into_bits() != 0 {
            // There are more replies pending, so handle that the next time around.
            PendingFutures::simple_rx(RxReady::Connection(self.instance_id()))
        } else if self.socket.is_closed() && self.local_send_shutdown {
            // The socket is fully closed, so prepare to send an RST if the guest hasn't by then.
            self.set_timeout(driver, GRACEFUL_SHUTDOWN_TIMEOUT)
        } else if self.state == ConnectionState::Connected {
            if self.peer_credit_available() > 0 {
                if self.local_send_shutdown {
                    tracing::debug!(?self.key, "waiting for connection close after local shutdown");
                    PendingFutures::rx(
                        self.socket
                            .await_close(RxReady::Connection(self.instance_id())),
                    )
                } else {
                    // No replies pending, so make sure we're waiting for data when the peer has
                    // credit.
                    PendingFutures::rx(
                        self.socket
                            .await_read_ready(RxReady::Connection(self.instance_id())),
                    )
                }
            } else if !self.waiting_for_credit {
                // The peer has no space left, so request an update.
                tracing::debug!(?self.key, "waiting for peer credit update");
                self.pending_reply.set_credit_request(true);
                self.waiting_for_credit = true;
                PendingFutures::simple_rx(RxReady::Connection(self.instance_id()))
            } else {
                // Already waiting for a credit update.
                PendingFutures::NONE
            }
        } else {
            PendingFutures::NONE
        };

        Ok((header, pending_work))
    }

    /// Handle data sent from the host on the relay socket.
    fn handle_host_data(
        &mut self,
        mem: &GuestMemory,
        payload: &[VirtioQueuePayload],
        guest_cid: u64,
    ) -> anyhow::Result<Option<VsockPacketBuf>> {
        // Check if we're allowed to send data to this connection.
        let peer_free = self.peer_credit_available();
        if peer_free == 0 {
            if self.waiting_for_credit {
                return Ok(None);
            }

            tracing::debug!(?self.key, "waiting for peer credit update");
            self.waiting_for_credit = true;
            return Ok(Some(new_reply_packet(
                self.key,
                Operation::CREDIT_REQUEST,
                guest_cid,
                self.fwd_cnt.0,
            )));
        }

        tracing::trace!(?self.key, peer_free, "peer buffer credit available");

        // Attempt to lock the payload buffers so we can write directly into them.
        let mut locked = lock_payload_data(
            mem,
            payload,
            peer_free.into(),
            false,
            true,
            LockedIoSliceMut::new(),
        )?;

        let (bytes_read, temp_buf) = if let Some(locked) = &mut locked {
            // We can read directly into the guest buffer.
            let bytes_read = self
                .socket
                .read_vectored(locked.get_mut().0.as_mut())
                .context("failed to read from host socket")?;

            (bytes_read, Vec::new())
        } else {
            // A temp bounce buffer is needed since the guest buffer couldn't be locked.
            let buf_len: usize = payload
                .iter()
                .filter_map(|p| p.writeable.then_some(p.length as usize))
                .sum();

            if buf_len < VSOCK_HEADER_SIZE {
                anyhow::bail!("guest buffer too small for vsock header");
            }

            let mut temp_buf = vec![0u8; buf_len - VSOCK_HEADER_SIZE];
            let bytes_read = self
                .socket
                .read(&mut temp_buf)
                .context("failed to read from host socket")?;
            if let Some(bytes_read) = bytes_read {
                temp_buf.truncate(bytes_read);
            }
            (bytes_read, temp_buf)
        };

        let Some(bytes_read) = bytes_read else {
            // No data available (would block).
            return Ok(None);
        };

        let packet = if bytes_read == 0 {
            tracing::debug!("host socket shutdown");
            self.local_send_shutdown = true;
            new_shutdown_packet(
                self.key,
                guest_cid,
                self.fwd_cnt.0,
                ShutdownFlags::new()
                    .with_send(true)
                    .with_receive(self.socket.is_closed()),
            )
        } else {
            tracing::trace!(bytes_read, "read data from host socket");
            self.tx_cnt += bytes_read as u32;
            new_rw_packet(
                self.key,
                guest_cid,
                self.fwd_cnt.0,
                bytes_read as u32,
                temp_buf,
            )
        };

        Ok(Some(packet))
    }

    fn handle_credit_update(&mut self, header: &VsockHeader) -> PendingFutures {
        self.peer_buf_alloc = header.buf_alloc;
        self.peer_fwd_cnt = header.fwd_cnt;
        if self.peer_credit_available() > 0 {
            self.waiting_for_credit = false;
            PendingFutures::rx(
                self.socket
                    .await_read_ready(RxReady::Connection(self.instance_id())),
            )
        } else {
            // Peer sent an update with zero bytes available for some reason, so request another
            // update.
            self.pending_reply.set_credit_request(true);
            PendingFutures::simple_rx(RxReady::Connection(self.instance_id()))
        }
    }

    /// Read and process the CONNECT request from the host, returning true if the entire request was
    /// received.
    fn read_relay_connect_request(&mut self) -> anyhow::Result<bool> {
        let ConnectionState::HostConnecting {
            buffer,
            read_write_count,
            use_guid,
        } = &mut self.state
        else {
            panic!("invalid state");
        };

        let Some(n) = self.socket.read(&mut buffer[*read_write_count..])? else {
            // No data available (would block).
            return Ok(false);
        };

        if n == 0 {
            anyhow::bail!("host socket closed before connection request was fully read");
        }

        *read_write_count += n;
        if buffer[*read_write_count - 1] != b'\n' {
            if *read_write_count == buffer.len() {
                anyhow::bail!("connect request too long");
            }

            // The request isn't complete yet.
            return Ok(false);
        }

        let request = VsockPortOrId::parse_connect_request(&buffer[..*read_write_count - 1])
            .context("failed to parse connect request")?;

        // GUID-style requests are allowed for consistency with the VMBus hvsocket relay, but since
        // we're connecting to a vsock, it must be using the vsock port template.
        let port = request.port().ok_or_else(|| {
            anyhow::anyhow!("connect request using non-vsock format: {request:?}")
        })?;

        // Make sure we send the OK message using the same format used for the request.
        *use_guid = Some(matches!(request, VsockPortOrId::Id(_)));

        tracing::debug!(port, "host connect request received");
        self.key.peer_port = port;
        Ok(true)
    }

    /// Handle the RESPONSE operation sent by the guest for a host-initiated connection, completing
    /// the connection establishment.
    fn handle_response(&mut self, header: &VsockHeader) -> anyhow::Result<PendingFutures> {
        let ConnectionState::HostConnecting {
            buffer,
            read_write_count: bytes_received,
            use_guid: Some(use_guid),
        } = &mut self.state
        else {
            anyhow::bail!("invalid state for RESPONSE");
        };

        // Store the initial buffer values.
        self.peer_buf_alloc = header.buf_alloc;
        self.peer_fwd_cnt = header.fwd_cnt;

        // Construct an OK response for the relay using the same format as the request.
        let response = if *use_guid {
            VsockPortOrId::Id(VsockPortOrId::port_to_id(self.key.local_port))
        } else {
            VsockPortOrId::Port(self.key.local_port)
        };

        let size = response.write_ok_response(buffer);
        buffer.truncate(size);
        *bytes_received = 0;

        // Attempt to write the OK response and complete the connection.
        self.complete_host_connection()
    }

    /// Attempts to finalize a host-initiated connection by writing the OK response.
    fn complete_host_connection(&mut self) -> anyhow::Result<PendingFutures> {
        let ConnectionState::HostConnecting {
            buffer,
            read_write_count: bytes_received,
            ..
        } = &mut self.state
        else {
            panic!("invalid state");
        };

        *bytes_received += self
            .socket
            .write(buffer)
            .context("failed to write OK response to host socket")?;
        if *bytes_received != buffer.len() {
            // Not all data was sent, wait until we can send the rest.
            return Ok(PendingFutures::new(
                self.socket.await_write_ready(self.instance_id()),
                None,
            ));
        }

        // The connection is now fully established.
        self.state = ConnectionState::Connected;
        self.timeout = None;
        let pending = if self.peer_credit_available() > 0 {
            // Begin checking for data to write to the guest.
            PendingFutures::rx(
                self.socket
                    .await_read_ready(RxReady::Connection(self.instance_id())),
            )
        } else {
            // Peer sent a response message with zero bytes available for some reason, so request
            // an update immediately.
            self.pending_reply.set_credit_request(true);
            PendingFutures::simple_rx(RxReady::Connection(self.instance_id()))
        };

        Ok(pending)
    }

    /// Calculate the peer's available buffer space based on the advertised buffer allocation, how
    /// much data we've sent, and how much the peer has forwarded from its buffer.
    fn peer_credit_available(&self) -> u32 {
        (Wrapping(self.peer_buf_alloc) - (self.tx_cnt - Wrapping(self.peer_fwd_cnt))).0
    }

    /// Set a timeout for this connection and return a future that will fire when the timeout
    /// expires.
    fn set_timeout(&mut self, driver: &VmTaskDriver, duration: Duration) -> PendingFutures {
        self.timeout = Some(Instant::now() + duration);
        let mut timer = PolledTimer::new(driver);
        let id = self.instance_id();
        PendingFutures::rx(Some(Box::pin(async move {
            timer.sleep(duration).await;
            RxReady::Connection(id)
        })))
    }

    /// Checks if the receive buffer is empty, including the case where it hasn't been allocated
    /// yet.
    fn is_recv_buf_empty(&self) -> bool {
        self.recv_buf.as_ref().is_none_or(|buf| buf.is_empty())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Ensure any pending read/write polls return.
        self.socket.shutdown(std::net::Shutdown::Both).ok();
    }
}

/// Manages connections for the virtio-vsock device.
pub struct ConnectionManager {
    guest_cid: u64,
    relay: UnixSocketRelay,
    conns: HashMap<ConnectionKey, Connection>,
    pending_conns: HashMap<u64, Connection>,
    next_seq: u64,
    local_ports: HashSet<u32>,
    last_local_port: u32,
}

impl ConnectionManager {
    /// Creates a new connection manager.
    ///
    /// `guest_cid` is the CID assigned to the guest.
    /// `base_path` is the directory path prefix for Unix sockets. For a vsock
    /// port P, the relay will try `<base_path>_P` first, then `<base_path>`.
    pub fn new(guest_cid: u64, base_path: PathBuf) -> Self {
        Self {
            guest_cid,
            relay: UnixSocketRelay::new(base_path),
            conns: HashMap::new(),
            pending_conns: HashMap::new(),
            next_seq: 0,
            local_ports: HashSet::new(),
            last_local_port: (1u32 << 30) - 1,
        }
    }

    /// Removes a connection.
    pub fn remove(&mut self, key: &ConnectionKey) {
        self.remove_connection(key);
    }

    /// Handle a new connection initiated by the host.
    pub fn handle_host_connect(
        &mut self,
        driver: &VmTaskDriver,
        stream: UnixStream,
    ) -> anyhow::Result<(PendingFutures, PendingFutures)> {
        if self.conns.len() + self.pending_conns.len() >= MAX_ACTIVE_CONNECTIONS {
            anyhow::bail!("maximum number of active connections reached");
        }

        let socket = RelaySocket::new(driver, stream)
            .context("Failed to create relay socket for incoming host connection")?;

        // Create a new connection.
        let seq = self.get_next_seq();
        let conn = Connection::new_host_initiated(self.allocate_local_port(), seq, socket);

        // Wait for read and handle the CONNECT request when that becomes available.
        let ready_future = PendingFutures::rx(
            conn.socket
                .await_read_ready(RxReady::PendingConnection(seq)),
        );

        let mut timer = PolledTimer::new(driver);
        let timeout_future = PendingFutures::rx(Some(Box::pin(async move {
            timer.sleep(CONNECTION_TIMEOUT).await;
            RxReady::PendingConnection(seq)
        })));

        assert!(self.pending_conns.insert(seq, conn).is_none());
        Ok((ready_future, timeout_future))
    }

    /// Handle a packet received from the guest on the tx virtqueue.
    pub fn handle_guest_tx(
        &mut self,
        driver: &VmTaskDriver,
        packet: VsockPacket<'_>,
    ) -> PendingFutures {
        match self.handle_guest_tx_inner(driver, packet) {
            Ok(pending) => pending,
            Err(err) => {
                tracelimit::warn_ratelimited!(%err, "error handling guest packet, sending reset");
                if err.remove {
                    self.remove_connection(&err.key);
                }
                PendingFutures::simple_rx(RxReady::SendReset(err.key))
            }
        }
    }

    /// Handle a packet received from the guest on the tx virtqueue.
    fn handle_guest_tx_inner(
        &mut self,
        driver: &VmTaskDriver,
        packet: VsockPacket<'_>,
    ) -> Result<PendingFutures, SendResetError> {
        let key = ConnectionKey::from_tx_packet(&packet.header);

        // Validate the packet. Only stream sockets are supported currently.
        let src_cid = packet.header.src_cid;
        let dst_cid = packet.header.dst_cid;
        if packet.header.socket_type() != SocketType::STREAM
            || src_cid != self.guest_cid
            || dst_cid != VSOCK_CID_HOST
        {
            return Err(SendResetError::new(
                key,
                format!(
                    "invalid packet from {src_cid} to {dst_cid}, type {:?}",
                    packet.header.socket_type()
                ),
            ));
        }

        let pending = match packet.header.operation() {
            Operation::REQUEST => self.handle_request_packet(driver, &packet.header, key)?,
            Operation::RESPONSE => self.handle_response_packet(key, &packet.header)?,
            Operation::RST => {
                if self.remove_connection(&key).is_some() {
                    tracing::debug!(?key, "guest reset connection");
                }
                PendingFutures::NONE
            }
            Operation::SHUTDOWN => self.handle_shutdown_packet(key, &packet.header)?,
            Operation::RW => self.handle_rw_packet(&packet, key)?,
            Operation::CREDIT_UPDATE => self
                .get_connection_mut(&key)?
                .handle_credit_update(&packet.header),
            Operation::CREDIT_REQUEST => {
                let conn = self.get_connection_mut(&key)?;
                conn.pending_reply.set_credit_update(true);
                PendingFutures::simple_rx(RxReady::Connection(conn.instance_id()))
            }
            op => {
                return Err(
                    SendResetError::new(key, format!("unsupported operation {op:?}"))
                        .with_remove(true),
                );
            }
        };

        Ok(pending)
    }

    /// Handles a RW packet from the guest.
    fn handle_rw_packet(
        &mut self,
        packet: &VsockPacket<'_>,
        key: ConnectionKey,
    ) -> Result<PendingFutures, SendResetError> {
        let conn = self.get_connection_mut(&key)?;
        let future = conn
            .handle_guest_data(packet.data, packet.header.len as usize)
            .map_err(|err| {
                SendResetError::new(key, "failed to handle RW from guest")
                    .with_inner(err)
                    .with_remove(true)
            })?;
        Ok(PendingFutures::new(
            future,
            conn.peer_needs_credit_update()
                .then_some(RxReady::Connection(conn.instance_id())),
        ))
    }

    /// Handles a SHUTDOWN packet from the guest.
    fn handle_shutdown_packet(
        &mut self,
        key: ConnectionKey,
        header: &VsockHeader,
    ) -> Result<PendingFutures, SendResetError> {
        let conn = self.get_connection_mut(&key)?;
        Ok(
            if let Err(err) = conn.handle_shutdown(header.shutdown_flags()) {
                tracelimit::warn_ratelimited!(
                    error = err.as_ref() as &dyn std::error::Error,
                    ?key,
                    "failed to shutdown connection"
                );

                PendingFutures::simple_rx(RxReady::SendReset(key))
            } else if conn.send_shutdown && conn.receive_shutdown && conn.is_recv_buf_empty() {
                // Both sides have shutdown and all buffered data has been forwarded, so we can
                // reset immediately.
                tracing::debug!(?key, "connection fully shutdown, removing");
                self.remove_connection(&key);
                PendingFutures::simple_rx(RxReady::SendReset(key))
            } else {
                PendingFutures::NONE
            },
        )
    }

    /// Handles a RESPONSE packet from the guest for a host-initiated connection.
    fn handle_response_packet(
        &mut self,
        key: ConnectionKey,
        header: &VsockHeader,
    ) -> Result<PendingFutures, SendResetError> {
        let conn = self.get_connection_mut(&key)?;
        conn.handle_response(header).map_err(|err| {
            SendResetError::new(key, "failed to handle RESPONSE from guest")
                .with_inner(err)
                .with_remove(true)
        })
    }

    /// Handles a REQUEST packet from the guest, initiating a new connection.
    fn handle_request_packet(
        &mut self,
        driver: &VmTaskDriver,
        header: &VsockHeader,
        key: ConnectionKey,
    ) -> Result<PendingFutures, SendResetError> {
        tracing::debug!(?header, "guest connect request");
        if self.conns.len() + self.pending_conns.len() >= MAX_ACTIVE_CONNECTIONS {
            return Err(SendResetError::new(
                key,
                "maximum number of active connections reached",
            ));
        }

        let socket = self.relay.connect(driver, key.local_port).map_err(|err| {
            SendResetError::new(
                key,
                format!("failed to connect to host socket for vsock request: {err}"),
            )
            .with_inner(err)
        })?;
        let seq = self.get_next_seq();
        match self.conns.entry(key) {
            Entry::Occupied(entry) => {
                // The guest is using a port combo that's already in use. Since that
                // indicates issues on the guest side, send a reset and drop the old
                // connection.
                entry.remove();
                return Err(SendResetError::new(
                    key,
                    "connect request for existing connection",
                ));
            }
            Entry::Vacant(entry) => entry.insert(Connection::new_guest_initiated(
                key,
                seq,
                header.buf_alloc,
                header.fwd_cnt,
                PendingControlPackets::new().with_respond(true),
                socket,
            )),
        };
        Ok(PendingFutures::simple_rx(RxReady::Connection(
            ConnectionInstanceId { key, seq },
        )))
    }

    /// Handle a write ready event for a connection, attempting to flush any buffered data to the relay.
    pub fn handle_write_ready(&mut self, id: ConnectionInstanceId) -> PendingFutures {
        let Some(conn) = self.conns.get_mut(&id.key) else {
            // This is fine if the connection was reset but a write future was still pending.
            tracing::debug!(?id, "write ready for unknown connection");
            return PendingFutures::NONE;
        };

        if id.seq != conn.seq {
            return PendingFutures::NONE;
        }

        // Check if we're still writing the OK response for a host-initiated connection.
        if matches!(conn.state, ConnectionState::HostConnecting { .. }) {
            return match conn.complete_host_connection() {
                Ok(value) => value,
                Err(err) => {
                    tracelimit::error_ratelimited!(
                        error = err.as_ref() as &dyn std::error::Error,
                        key = ?id.key,
                        "failed to complete connection from host"
                    );
                    self.remove_connection(&id.key);
                    PendingFutures::simple_rx(RxReady::SendReset(id.key))
                }
            };
        }

        match conn.write_from_buffer() {
            Ok(future) => {
                if conn.send_shutdown && conn.receive_shutdown && conn.is_recv_buf_empty() {
                    tracing::debug!(?id, "connection fully shutdown after write, removing");
                    self.remove_connection(&id.key);
                    return PendingFutures::simple_rx(RxReady::SendReset(id.key));
                }

                PendingFutures::new(
                    future,
                    conn.peer_needs_credit_update()
                        .then_some(RxReady::Connection(conn.instance_id())),
                )
            }
            Err(err) => {
                tracelimit::warn_ratelimited!(
                    error = err.as_ref() as &dyn std::error::Error,
                    ?id,
                    "failed to write buffered data to host socket on write ready"
                );
                PendingFutures::simple_rx(RxReady::SendReset(id.key))
            }
        }
    }

    /// Retrieve a packet to put on the RX queue for a connection that's ready.
    pub fn get_rx_packet(
        &mut self,
        mem: &GuestMemory,
        driver: &VmTaskDriver,
        payload: &[VirtioQueuePayload],
        rx_ready: RxReady,
    ) -> (Option<VsockPacketBuf>, PendingFutures) {
        match rx_ready {
            RxReady::Connection(id) => {
                let Some(conn) = self.conns.get_mut(&id.key) else {
                    return (None, PendingFutures::NONE);
                };

                if conn.seq != id.seq {
                    return (None, PendingFutures::NONE);
                }

                match conn.get_rx_packet(driver, mem, self.guest_cid, payload) {
                    Ok((packet, pending_work)) => (packet, pending_work),
                    Err(err) => {
                        tracelimit::warn_ratelimited!(
                            error = err.as_ref() as &dyn std::error::Error,
                            ?id,
                            "packet rx error"
                        );
                        self.remove_connection(&id.key);
                        (
                            Some(new_rst_packet(self.guest_cid, id.key)),
                            PendingFutures::NONE,
                        )
                    }
                }
            }
            RxReady::PendingConnection(seq) => {
                let Some(conn) = self.pending_conns.get_mut(&seq) else {
                    // This can happen if e.g. the timeout fires after the event connected.
                    return (None, PendingFutures::NONE);
                };

                if conn.timeout.is_some_and(|t| Instant::now() >= t) {
                    tracing::debug!(seq, "pending connection timed out");
                    self.remove_pending_connection(seq);
                    return (None, PendingFutures::NONE);
                }

                let ready = match conn.read_relay_connect_request() {
                    Ok(ready) => ready,
                    Err(err) => {
                        tracelimit::warn_ratelimited!(
                            error = err.as_ref() as &dyn std::error::Error,
                            seq,
                            "failed to read connect request from host socket"
                        );
                        self.remove_pending_connection(seq);
                        return (None, PendingFutures::NONE);
                    }
                };

                // Check if the entire CONNECT message was received.
                if ready {
                    // Do not use remove_pending_connection because the port should stay allocated.
                    let conn = self.pending_conns.remove(&seq).unwrap();
                    let key = conn.key;
                    match self.conns.entry(conn.key) {
                        Entry::Occupied(_) => {
                            // I don't think this can't normally happen since it would require for
                            // the guest to accept a connection on a port that it has also used to
                            // connect to a host listener. If it does, we reject this connection so
                            // the host socket will close, and keep the existing connection.
                            tracelimit::warn_ratelimited!(
                                ?conn.key,
                                "pending connection for existing connection"
                            );
                            (None, PendingFutures::NONE)
                        }
                        Entry::Vacant(entry) => {
                            entry.insert(conn);
                            (
                                Some(new_reply_packet(key, Operation::REQUEST, self.guest_cid, 0)),
                                PendingFutures::NONE,
                            )
                        }
                    }
                } else {
                    // Not ready yet, so wait for more data.
                    (
                        None,
                        PendingFutures::rx(
                            conn.socket
                                .await_read_ready(RxReady::PendingConnection(seq)),
                        ),
                    )
                }
            }
            RxReady::SendReset(key) => {
                assert!(
                    !self.conns.contains_key(&key),
                    "connection should have been removed"
                );

                (
                    Some(new_rst_packet(self.guest_cid, key)),
                    PendingFutures::NONE,
                )
            }
        }
    }

    /// Get the next connection sequence number.
    fn get_next_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }

    /// Allocate a local port for a host-initiated connection.
    fn allocate_local_port(&mut self) -> u32 {
        // The bounded maximum number of connections means that this will always find a port.
        loop {
            self.last_local_port = (self.last_local_port + 1) & !(1 << 31) | (1 << 30);

            // Using a HashSet is probably more efficient than anything else for the expected small
            // number of connections.
            // N.B. This does not avoid collission with listening sockets, though the high range
            //      used should make that unlikely. In case it does happen, the guest should still
            //      use a different source port so the connection key is still unique.
            if self.local_ports.insert(self.last_local_port) {
                break;
            }
        }

        self.last_local_port
    }

    /// Free a previously allocated local port for a connection.
    fn free_local_port(&mut self, conn: Option<&Connection>) {
        if let Some(conn) = conn {
            if conn.allocated_local_port {
                self.local_ports.remove(&conn.key.local_port);
            }
        }
    }

    /// Remove a connection and free its local port if it was allocated, returning the connection.
    fn remove_connection(&mut self, key: &ConnectionKey) -> Option<Connection> {
        let conn = self.conns.remove(key);
        self.free_local_port(conn.as_ref());
        conn
    }

    /// Remove a pending connection and free its local port if it was allocated.
    fn remove_pending_connection(&mut self, seq: u64) {
        let conn = self.pending_conns.remove(&seq);
        self.free_local_port(conn.as_ref());
    }

    /// Get a connection by key. This is a convenience function so we can ensure a reset is sent if
    /// the connection isn't found.
    fn get_connection_mut(
        &mut self,
        key: &ConnectionKey,
    ) -> Result<&mut Connection, SendResetError> {
        self.conns
            .get_mut(key)
            .ok_or_else(|| SendResetError::new(*key, "connection not found"))
    }
}

/// Construct a new reply packet that doesn't need additional fields set.
fn new_reply_packet(
    key: ConnectionKey,
    op: Operation,
    guest_cid: u64,
    fwd_cnt: u32,
) -> VsockPacketBuf {
    VsockPacketBuf::header_only(VsockHeader {
        src_cid: VSOCK_CID_HOST,
        dst_cid: guest_cid,
        src_port: key.local_port,
        dst_port: key.peer_port,
        len: 0,
        socket_type: SocketType::STREAM.0,
        op: op.0,
        flags: ShutdownFlags::new().into(),
        buf_alloc: TX_BUF_SIZE,
        fwd_cnt,
    })
}

/// Construct a new RW packet with the given data.
fn new_rw_packet(
    key: ConnectionKey,
    guest_cid: u64,
    fwd_cnt: u32,
    len: u32,
    data: Vec<u8>,
) -> VsockPacketBuf {
    VsockPacketBuf::new(
        VsockHeader {
            src_cid: VSOCK_CID_HOST,
            dst_cid: guest_cid,
            src_port: key.local_port,
            dst_port: key.peer_port,
            len,
            socket_type: SocketType::STREAM.0,
            op: Operation::RW.0,
            flags: ShutdownFlags::new().into(),
            buf_alloc: TX_BUF_SIZE,
            fwd_cnt,
        },
        data,
    )
}

/// Construct a new SHUTDOWN packet with the given flags.
fn new_shutdown_packet(
    key: ConnectionKey,
    guest_cid: u64,
    fwd_cnt: u32,
    flags: ShutdownFlags,
) -> VsockPacketBuf {
    VsockPacketBuf::header_only(VsockHeader {
        src_cid: VSOCK_CID_HOST,
        dst_cid: guest_cid,
        src_port: key.local_port,
        dst_port: key.peer_port,
        len: 0,
        socket_type: SocketType::STREAM.0,
        op: Operation::SHUTDOWN.0,
        flags: flags.into(),
        buf_alloc: TX_BUF_SIZE,
        fwd_cnt,
    })
}

/// Construct a new RST packet, which doesn't include credit information.
fn new_rst_packet(guest_cid: u64, key: ConnectionKey) -> VsockPacketBuf {
    VsockPacketBuf::header_only(VsockHeader {
        src_cid: VSOCK_CID_HOST,
        dst_cid: guest_cid,
        src_port: key.local_port,
        dst_port: key.peer_port,
        len: 0,
        socket_type: SocketType::STREAM.0,
        op: Operation::RST.0,
        flags: ShutdownFlags::new().into(),
        buf_alloc: 0,
        fwd_cnt: 0,
    })
}

/// Error that indicates a reset should be sent to the guest with the specified key.
#[derive(Debug)]
struct SendResetError {
    key: ConnectionKey,
    message: String,
    inner: Option<anyhow::Error>,
    /// Indicates the connection should also be removed.
    remove: bool,
}

impl std::fmt::Display for SendResetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; key = {:?}", self.message, self.key)?;
        if let Some(inner) = &self.inner {
            write!(f, "; inner = {}", inner)?;
        }

        Ok(())
    }
}

impl SendResetError {
    fn new(key: ConnectionKey, message: impl Into<String>) -> Self {
        Self {
            key,
            message: message.into(),
            inner: None,
            remove: true,
        }
    }

    fn with_inner(mut self, inner: anyhow::Error) -> Self {
        self.inner = Some(inner);
        self
    }

    fn with_remove(mut self, remove: bool) -> Self {
        self.remove = remove;
        self
    }
}
