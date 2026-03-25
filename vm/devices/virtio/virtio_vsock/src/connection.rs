// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unix socket relay for virtio-vsock connections.
//!
//! This module implements a relay between vsock connections from the guest and
//! Unix domain sockets on the host, following a model similar to the hybrid
//! vsock approach used for Hyper-V sockets.
//!
//! When the guest connects to a vsock port, the relay looks up a Unix domain
//! socket path based on the port number (e.g., `<base_path>_<port>`) and
//! establishes a bidirectional data relay between the guest's vsock stream and
//! the host's Unix socket.
//!
//! The relay also supports listening mode: a Unix listener socket accepts
//! host-initiated connections and routes them into the guest via a
//! `CONNECT <port>` text protocol (the same hybrid vsock protocol used by
//! Firecracker and others).

use crate::LockedIoSliceMut;
use crate::PendingWork;
use crate::WriteReadyItem;
use crate::lock_payload_data;
use crate::ring::RingBuffer;
use crate::spec::Operation;
use crate::spec::ShutdownFlags;
use crate::spec::SocketType;
use crate::spec::VSOCK_CID_HOST;
use crate::spec::VsockHeader;
use crate::spec::VsockPacket;
use crate::unix_relay::RelaySocket;
use crate::unix_relay::UnixSocketRelay;
use anyhow::Context;
use bitfield_struct::bitfield;
use guestmem::GuestMemory;
use hybrid_vsock::ConnectionRequest;
use hybrid_vsock::HYBRID_CONNECT_REQUEST_LEN;
use pal_async::interest::InterestSlot;
use pal_async::timer::Instant;
use pal_async::timer::PolledTimer;
use std::collections::HashMap;
use std::io::IoSlice;
use std::num::Wrapping;
use std::path::PathBuf;
use std::time::Duration;
use unix_socket::UnixStream;
use virtio::queue::VirtioQueuePayload;
use vmcore::vm_task::VmTaskDriver;

const TX_BUF_SIZE: u32 = 65536;
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(2);

/// A key that uniquely identifies a vsock connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnKey {
    local_port: u32,
    peer_port: u32,
}

impl ConnKey {
    pub fn from_tx_packet(hdr: &VsockHeader) -> Self {
        Self {
            local_port: hdr.dst_port,
            peer_port: hdr.src_port,
        }
    }
}

/// A connection key combined with a sequence number to distinguish connections when a port is
/// reused, in case some futures for the old connection may still be pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionInstanceId {
    pub key: ConnKey,
    pub seq: u64,
}

#[bitfield(u32)]
struct PendingReply {
    reset: bool,
    respond: bool,
    credit_request: bool,
    credit_update: bool,
    #[bits(28)]
    _reserved: u32,
}

#[derive(Debug, PartialEq, Eq)]
enum ConnectionState {
    PreHostConnect {
        buffer: Box<[u8]>,
        bytes_received: usize,
    },
    PostHostConnect,
    GuestConnecting,
    Connected,
}

/// Tracks the state of a single vsock connection relayed to a Unix socket.
struct Connection {
    key: ConnKey,
    seq: u64,
    state: ConnectionState,
    /// Buffer allocation advertised by the peer (guest).
    peer_buf_alloc: u32,
    /// Received data that the peer has forwarded from its buffer.
    peer_fwd_cnt: u32,
    tx_cnt: Wrapping<u32>,
    /// Data received from the peer that has been forwarded to the unix socket relay.
    fwd_cnt: Wrapping<u32>,
    last_sent_fwd_count: u32,
    pending_reply: PendingReply,
    socket: RelaySocket,
    recv_buf: Option<RingBuffer>,
    timeout: Option<Instant>,
    send_shutdown: bool,
    receive_shutdown: bool,
    local_send_shutdown: bool,
}

impl Connection {
    fn new(
        key: ConnKey,
        seq: u64,
        peer_buf_alloc: u32,
        peer_fwd_cnt: u32,
        pending_reply: PendingReply,
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
        }
    }

    fn new_pending(local_port: u32, seq: u64, socket: RelaySocket) -> Self {
        Self {
            key: ConnKey {
                local_port,
                // Not known yet.
                peer_port: 0,
            },
            seq,
            state: ConnectionState::PreHostConnect {
                buffer: Box::new([0; HYBRID_CONNECT_REQUEST_LEN]),
                bytes_received: 0,
            },
            peer_buf_alloc: 0,
            peer_fwd_cnt: 0,
            tx_cnt: Wrapping(0),
            fwd_cnt: Wrapping(0),
            last_sent_fwd_count: 0,
            pending_reply: PendingReply::new(),
            socket,
            recv_buf: None,
            timeout: Some(Instant::now() + CONNECTION_TIMEOUT),
            send_shutdown: false,
            receive_shutdown: false,
            local_send_shutdown: false,
        }
    }

    fn instance_id(&self) -> ConnectionInstanceId {
        ConnectionInstanceId {
            key: self.key,
            seq: self.seq,
        }
    }

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

        tracing::info!(remaining, "buffering data from guest");
        buf.write(data, bytes_sent);
        Ok(self.socket.await_write_ready(self.instance_id()))
    }

    fn write_from_buffer(&mut self) -> anyhow::Result<Option<WriteReadyItem>> {
        let ring = self.recv_buf.as_mut().expect("buffer must exist");
        let sent = self
            .socket
            .write_from_ring(ring)
            .context("failed to write buffered data to relay socket")?;

        tracing::info!(sent, "forwarded buffered data to relay socket");

        self.fwd_cnt += sent as u32;
        if ring.is_empty() {
            // TODO: Check if I do this when shutdown is received.
            // Check if the peer has already sent a shutdown that we can forward to the host now
            // that the buffer is empty.
            if self.send_shutdown {
                self.socket
                    .shutdown(std::net::Shutdown::Write)
                    .context("failed to shutdown write side of socket")?;
            }

            Ok(None)
        } else {
            tracing::trace!(
                ring_len = ring.len(),
                "write buffer not empty after write, waiting for write ready"
            );
            Ok(self.socket.await_write_ready(self.instance_id()))
        }
    }

    fn peer_needs_credit_update(&self) -> bool {
        self.fwd_cnt.0 != self.last_sent_fwd_count
    }

    fn shutdown(&mut self, mut flags: ShutdownFlags) -> anyhow::Result<PendingWork> {
        if self.state != ConnectionState::Connected {
            anyhow::bail!("peer sent shutdown before connection established");
        }

        if flags.send() {
            // Don't actually shutdown the host socket write if we're still waiting to flush data
            // out of the buffer.
            if !self.is_recv_buf_empty() {
                tracing::info!("deferring send shutdown until buffer is flushed");
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
            tracing::info!(?how, "peer initiated shutdown");
            self.socket.shutdown(how)?;
        }

        Ok(PendingWork::NONE)
    }

    fn handle_host_data(
        &mut self,
        mem: &GuestMemory,
        payload: &[VirtioQueuePayload],
        guest_cid: u64,
    ) -> anyhow::Result<Option<VsockHeader>> {
        let peer_free = self.peer_credit_available();
        if peer_free == 0 {
            tracing::info!("peer has no buffer credit available, waiting for credit update");
            return Ok(Some(new_reply_packet(
                self.key,
                Operation::CREDIT_REQUEST,
                guest_cid,
                self.fwd_cnt.0,
            )));
        }

        tracing::info!(peer_free, "peer buffer credit available");
        let mut locked = lock_payload_data(
            mem,
            payload,
            peer_free.into(),
            false,
            true,
            LockedIoSliceMut(Vec::new()),
        )?;

        let Some(bytes_read) = self
            .socket
            .read_vectored(locked.get_mut().0.as_mut())
            .context("failed to read from host socket")?
        else {
            // No data available (would block).
            return Ok(None);
        };

        let packet = if bytes_read == 0 {
            tracing::debug!("host socket shutdown");
            self.local_send_shutdown = true;
            self.socket.clear_ready(InterestSlot::Read);
            new_shutdown_packet(
                self.key,
                guest_cid,
                self.fwd_cnt.0,
                ShutdownFlags::new().with_send(true),
            )
        } else {
            tracing::trace!(bytes_read, "read data from host socket");
            self.tx_cnt += bytes_read as u32;
            new_rw_packet(self.key, guest_cid, self.fwd_cnt.0, bytes_read as u32)
        };

        Ok(Some(packet))
    }

    fn handle_read_connect(&mut self) -> anyhow::Result<bool> {
        let ConnectionState::PreHostConnect {
            buffer,
            bytes_received,
        } = &mut self.state
        else {
            panic!("handle_read_connect called in invalid state");
        };

        // TODO: Use the read that handles WouldBlock.
        let Some(n) = self.socket.read(&mut buffer[*bytes_received..])? else {
            // No data available (would block).
            return Ok(false);
        };

        if n == 0 {
            anyhow::bail!("host socket closed before connection request was fully read");
        }

        *bytes_received += n;
        if buffer[*bytes_received - 1] != b'\n' {
            if *bytes_received == buffer.len() {
                anyhow::bail!("connect request too long");
            }

            tracing::trace!(
                bytes_received,
                "partial connect request received, waiting for more data"
            );
            return Ok(false);
        }

        let request = ConnectionRequest::parse_connect_request(&buffer[..*bytes_received - 1])
            .context("failed to parse connect request")?;

        let port = request.port().ok_or_else(|| {
            anyhow::anyhow!("connect request using non-vsock format: {request:?}")
        })?;

        tracing::trace!(port, "host connect request received");
        self.key.peer_port = port;
        self.state = ConnectionState::PostHostConnect;
        Ok(true)
    }

    /// Calculate the peer's available buffer space based on the advertised buffer allocation, how
    /// much data we've sent, and how much the peer has forwarded from its buffer.
    fn peer_credit_available(&self) -> u32 {
        (Wrapping(self.peer_buf_alloc) - (self.tx_cnt - Wrapping(self.peer_fwd_cnt))).0
    }

    fn set_timeout(&mut self, driver: &VmTaskDriver, duration: Duration) -> PendingWork {
        self.timeout = Some(Instant::now() + duration);
        let mut timer = PolledTimer::new(driver);
        let id = self.instance_id();
        PendingWork::rx(Some(Box::pin(async move {
            timer.sleep(duration).await;
            RxWork::Connection(id)
        })))
    }

    fn is_recv_buf_empty(&self) -> bool {
        self.recv_buf.as_ref().is_none_or(|buf| buf.is_empty())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Shutdown the socket so any pending read/write polls will complete.
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
    }
}

/// Manages vsock-to-Unix-socket relaying for a virtio-vsock device.
///
/// Connections initiated by the guest are relayed to Unix domain sockets on the
/// host. The relay looks up the socket path as `<base_path>_<port>` for each
/// destination port.
pub struct ConnectionManager {
    guest_cid: u64,
    relay: UnixSocketRelay,
    conns: HashMap<ConnKey, Connection>,
    pending_conns: HashMap<u64, Connection>,
    next_seq: u64,
}

impl ConnectionManager {
    /// Creates a new relay.
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
        }
    }

    pub fn handle_host_connect(
        &mut self,
        driver: &VmTaskDriver,
        stream: UnixStream,
    ) -> anyhow::Result<(PendingWork, PendingWork)> {
        let socket = RelaySocket::new(driver, stream)
            .context("Failed to create relay socket for incoming host connection")?;
        let seq = self.next_seq;
        // TODO: Allocate a local port.

        let conn = Connection::new_pending(1234, seq, socket);

        let read_work =
            PendingWork::rx(conn.socket.await_read_ready(RxWork::PendingConnection(seq)));

        let mut timer = PolledTimer::new(driver);
        let timeout_work = PendingWork::rx(Some(Box::pin(async move {
            timer.sleep(CONNECTION_TIMEOUT).await;
            RxWork::PendingConnection(seq)
        })));

        assert!(self.pending_conns.insert(seq, conn).is_none());
        Ok((read_work, timeout_work))
    }

    /// Handle a packet received from the guest on the tx virtqueue.
    pub fn handle_guest_tx(
        &mut self,
        driver: &VmTaskDriver,
        packet: VsockPacket<'_>,
    ) -> PendingWork {
        let key = ConnKey::from_tx_packet(&packet.header);

        // Validate the packet. Only stream sockets are supported currently.
        if SocketType(packet.header.socket_type) != SocketType::STREAM
            || packet.header.src_cid != self.guest_cid
        {
            tracing::debug!(
                header = ?packet.header,
                guest_cid = self.guest_cid,
                "invalid source CID"
            );

            return PendingWork::simple_rx(RxWork::SendReset(key));
        }

        let op = Operation(packet.header.op);
        match op {
            Operation::REQUEST => {
                tracing::info!(?packet.header, "connect request");
                // Guest is initiating a connection to a port on the host.
                match self.relay.connect(driver, key.local_port) {
                    Ok(socket) => {
                        // TODO: Handle existing connection
                        let seq = self.get_next_seq();
                        self.conns.insert(
                            key,
                            Connection::new(
                                key,
                                seq,
                                packet.header.buf_alloc,
                                packet.header.fwd_cnt,
                                PendingReply::new().with_respond(true),
                                socket,
                            ),
                        );

                        PendingWork::simple_rx(RxWork::Connection(ConnectionInstanceId {
                            key,
                            seq,
                        }))
                    }
                    Err(err) => {
                        tracelimit::warn_ratelimited!(
                            error = err.as_ref() as &dyn std::error::Error,
                            port = key.local_port,
                            "failed to connect to host socket for vsock request"
                        );
                        PendingWork::simple_rx(RxWork::SendReset(key))
                    }
                }
            }
            Operation::RESPONSE => {
                // Guest is accepting a host-initiated connection.
                // Update credit info and mark connection as established.
                let Some(conn) = self.conns.get_mut(&key) else {
                    tracelimit::warn_ratelimited!(?key, "RESPONSE for unknown connection");
                    return PendingWork::simple_rx(RxWork::SendReset(key));
                };

                conn.peer_buf_alloc = packet.header.buf_alloc;
                conn.peer_fwd_cnt = packet.header.fwd_cnt;
                conn.state = ConnectionState::Connected;
                conn.timeout = None;

                if conn.peer_credit_available() > 0 {
                    PendingWork::rx(
                        conn.socket
                            .await_read_ready(RxWork::Connection(conn.instance_id())),
                    )
                } else {
                    // Peer sent a response with zero bytes available for some reason, so request
                    // an update.
                    conn.pending_reply.with_credit_request(true);
                    PendingWork::simple_rx(RxWork::Connection(conn.instance_id()))
                }
            }
            Operation::RST => {
                if let Some(_conn) = self.conns.remove(&key) {
                    tracing::debug!(?key, "guest reset connection");
                }
                PendingWork::NONE
            }
            Operation::SHUTDOWN => {
                let Some(conn) = self.conns.get_mut(&key) else {
                    tracelimit::warn_ratelimited!(?key, "SHUTDOWN for unknown connection");
                    return PendingWork::simple_rx(RxWork::SendReset(key));
                };

                if let Err(err) = conn.shutdown(ShutdownFlags::from_bits(packet.header.flags)) {
                    tracelimit::warn_ratelimited!(
                        error = err.as_ref() as &dyn std::error::Error,
                        ?key,
                        "failed to shutdown connection"
                    );

                    PendingWork::simple_rx(RxWork::SendReset(key))
                } else if conn.send_shutdown && conn.receive_shutdown && conn.is_recv_buf_empty() {
                    // Both sides have shutdown and all buffered data has been forwarded, so we can
                    // reset immediately.
                    tracing::info!(?key, "connection fully shutdown, removing");
                    self.conns.remove(&key);
                    PendingWork::simple_rx(RxWork::SendReset(key))
                } else {
                    PendingWork::NONE
                }
            }
            Operation::RW => {
                // Guest is sending data.
                // TODO: Use a custom error type so handle reset can be more easily propagated.
                let Some(conn) = self.conns.get_mut(&key) else {
                    tracelimit::warn_ratelimited!(?key, "RW for unknown connection");
                    return PendingWork::simple_rx(RxWork::SendReset(key));
                };

                match conn.handle_guest_data(packet.data, packet.header.len as usize) {
                    Ok(future) => PendingWork::new(
                        future,
                        conn.peer_needs_credit_update()
                            .then_some(RxWork::Connection(conn.instance_id())),
                    ),
                    Err(err) => {
                        tracelimit::warn_ratelimited!(
                            error = err.as_ref() as &dyn std::error::Error,
                            ?key,
                            "failed to write guest data to host socket"
                        );
                        PendingWork::simple_rx(RxWork::SendReset(key))
                    }
                }
            }
            Operation::CREDIT_UPDATE => {
                let Some(conn) = self.conns.get_mut(&key) else {
                    tracelimit::warn_ratelimited!(?key, "CREDIT_UPDATE for unknown connection");
                    return PendingWork::simple_rx(RxWork::SendReset(key));
                };

                conn.peer_buf_alloc = packet.header.buf_alloc;
                conn.peer_fwd_cnt = packet.header.fwd_cnt;
                if conn.peer_credit_available() > 0 {
                    PendingWork::rx(
                        conn.socket
                            .await_read_ready(RxWork::Connection(conn.instance_id())),
                    )
                } else {
                    // Peer sent an update with zero bytes available for some reason, so request
                    // another update.
                    conn.pending_reply.with_credit_request(true);
                    PendingWork::simple_rx(RxWork::Connection(conn.instance_id()))
                }
            }
            Operation::CREDIT_REQUEST => {
                let Some(conn) = self.conns.get_mut(&key) else {
                    tracelimit::warn_ratelimited!(?key, "CREDIT_REQUEST for unknown connection");
                    return PendingWork::simple_rx(RxWork::SendReset(key));
                };

                conn.pending_reply.set_credit_update(true);
                PendingWork::simple_rx(RxWork::Connection(conn.instance_id()))
            }
            _ => {
                tracing::debug!(header = ?packet.header, "unknown vsock operation");
                // TODO: Send RST for unknown operations?
                PendingWork::NONE
            }
        }
    }

    pub fn handle_write_ready(&mut self, id: ConnectionInstanceId) -> PendingWork {
        let Some(conn) = self.conns.get_mut(&id.key) else {
            // This is fine if the connection was reset but a write future was still pending.
            tracing::debug!(?id, "write ready for unknown connection");
            return PendingWork::NONE;
        };

        if id.seq != conn.seq {
            return PendingWork::NONE;
        }

        match conn.write_from_buffer() {
            Ok(future) => {
                if conn.send_shutdown && conn.is_recv_buf_empty() {
                    if let Err(err) = conn.socket.shutdown(std::net::Shutdown::Write) {
                        tracelimit::warn_ratelimited!(
                            error = &err as &dyn std::error::Error,
                            ?id,
                            "failed to shutdown write side of socket after flushing buffer"
                        );

                        self.conns.remove(&id.key);
                        return PendingWork::simple_rx(RxWork::SendReset(id.key));
                    }

                    if conn.receive_shutdown {
                        tracing::info!(?id, "connection fully shutdown after write, removing");
                        self.conns.remove(&id.key);
                        return PendingWork::simple_rx(RxWork::SendReset(id.key));
                    }
                }

                PendingWork::new(
                    future,
                    conn.peer_needs_credit_update()
                        .then_some(RxWork::Connection(conn.instance_id())),
                )
            }
            Err(err) => {
                tracelimit::warn_ratelimited!(
                    error = err.as_ref() as &dyn std::error::Error,
                    ?id,
                    "failed to write buffered data to host socket on write ready"
                );
                PendingWork::simple_rx(RxWork::SendReset(id.key))
            }
        }
    }

    // fn handle_connect_request(
    //     &mut self,
    //     key: ConnKey,
    //     peer_buf_alloc: u32,
    //     peer_fwd_cnt: u32,
    // ) -> VsockHeader {
    //     // TODO: Actually connect.
    //     // // Check if we can connect to a Unix socket for this port.
    //     // let path = match self.host_uds_path(key.local_port) {
    //     //     Ok(p) => p,
    //     //     Err(err) => {
    //     //         tracelimit::warn_ratelimited!(
    //     //             error = err.as_ref() as &dyn std::error::Error,
    //     //             key.local_port,
    //     //             "no host socket for vsock port"
    //     //         );
    //     //         self.queue_reply(conn, key, PendingReply::new().with_reset(true));
    //     //         return;
    //     //     }
    //     // };

    //     // TODO: Check for collission
    //     self.conns.insert(
    //         key,
    //         Connection {
    //             key,
    //             peer_buf_alloc,
    //             peer_fwd_cnt,
    //             pending_reply: PendingReply::new()
    //         },
    //     );

    //     // Send RESPONSE to accept the connection.
    //     self.new_reply_packet(key, Operation::RESPONSE)
    // }

    pub fn get_rx_packet(
        &mut self,
        mem: &GuestMemory,
        driver: &VmTaskDriver,
        payload: &[VirtioQueuePayload],
        work: RxWork,
    ) -> (Option<VsockHeader>, PendingWork) {
        match work {
            RxWork::Connection(id) => {
                let Some(conn) = self.conns.get_mut(&id.key) else {
                    return (None, PendingWork::NONE);
                };

                if conn.seq != id.seq {
                    return (None, PendingWork::NONE);
                }

                if let Some(timeout) = conn.timeout {
                    if Instant::now() >= timeout {
                        tracing::info!(?id, "connection timed out");
                        self.conns.remove(&id.key);
                        return (
                            Some(new_rst_packet(self.guest_cid, id.key)),
                            PendingWork::NONE,
                        );
                    }
                }

                // TODO: Make a function in Connection for this.
                let header = if conn.pending_reply.reset() {
                    // Remove the connection immediately on reset.
                    self.conns.remove(&id.key);
                    return (
                        Some(new_rst_packet(self.guest_cid, id.key)),
                        PendingWork::NONE,
                    );
                } else if conn.pending_reply.respond() {
                    conn.pending_reply.set_respond(false);
                    conn.state = ConnectionState::Connected;
                    conn.last_sent_fwd_count = conn.fwd_cnt.0;

                    Some(new_reply_packet(
                        id.key,
                        Operation::RESPONSE,
                        self.guest_cid,
                        conn.fwd_cnt.0,
                    ))
                } else if conn.peer_needs_credit_update() || conn.pending_reply.credit_update() {
                    conn.last_sent_fwd_count = conn.fwd_cnt.0;
                    let fwd_cnt = conn.fwd_cnt.0;

                    conn.pending_reply.set_credit_update(false);
                    tracing::info!(?id.key, fwd_cnt, "sending credit update");
                    Some(new_reply_packet(
                        id.key,
                        Operation::CREDIT_UPDATE,
                        self.guest_cid,
                        conn.fwd_cnt.0,
                    ))
                } else if conn.pending_reply.credit_request() {
                    conn.pending_reply.set_credit_request(false);
                    Some(new_reply_packet(
                        id.key,
                        Operation::CREDIT_REQUEST,
                        self.guest_cid,
                        conn.fwd_cnt.0,
                    ))
                } else if conn.socket.has_data() {
                    assert_eq!(conn.pending_reply.into_bits(), 0);
                    match conn.handle_host_data(mem, payload, self.guest_cid) {
                        Ok(header) => header,
                        Err(err) => {
                            tracelimit::warn_ratelimited!(
                                error = err.as_ref() as &dyn std::error::Error,
                                ?id.key,
                                "failed to read data from host socket"
                            );
                            self.conns.remove(&id.key);
                            return (
                                Some(new_rst_packet(self.guest_cid, id.key)),
                                PendingWork::NONE,
                            );
                        }
                    }
                } else {
                    assert_eq!(conn.pending_reply.into_bits(), 0);
                    None
                };

                let pending_work = if conn.pending_reply.into_bits() != 0 {
                    // More replies pending, so handle that the next time around.
                    PendingWork::simple_rx(RxWork::Connection(id))
                } else if conn.socket.is_closed() {
                    if conn.local_send_shutdown {
                        conn.set_timeout(driver, GRACEFUL_SHUTDOWN_TIMEOUT)
                    } else {
                        // Socket closed without us shutting down the write side, so reset immediately.
                        self.conns.remove(&id.key);
                        PendingWork::simple_rx(RxWork::SendReset(id.key))
                    }
                } else if conn.state == ConnectionState::Connected
                    && conn.peer_credit_available() > 0
                {
                    // No replies pending, so make sure we're waiting for data.
                    // N.B. This is done even if the socket was shutdown because this is how we find
                    //      out if it was closed or has an error if there is no write pending.
                    PendingWork::rx(conn.socket.await_read_ready(RxWork::Connection(id)))
                } else {
                    PendingWork::NONE
                };

                // TODO: Check for socket data
                (header, pending_work)
            }
            RxWork::PendingConnection(seq) => {
                let Some(conn) = self.pending_conns.get_mut(&seq) else {
                    // This can happen if e.g. the timeout fires after the event connected.
                    return (None, PendingWork::NONE);
                };

                if conn.timeout.is_some_and(|t| Instant::now() >= t) {
                    tracing::debug!(seq, "pending connection timed out");
                    self.pending_conns.remove(&seq);
                    return (None, PendingWork::NONE);
                }

                let ready = match conn.handle_read_connect() {
                    Ok(ready) => ready,
                    Err(err) => {
                        tracelimit::warn_ratelimited!(
                            error = err.as_ref() as &dyn std::error::Error,
                            seq,
                            "failed to read connect request from host socket"
                        );
                        self.pending_conns.remove(&seq);
                        return (None, PendingWork::NONE);
                    }
                };

                if ready {
                    let conn = self.pending_conns.remove(&seq).unwrap();
                    let key = conn.key;
                    self.conns.insert(conn.key, conn);
                    (
                        Some(new_reply_packet(key, Operation::REQUEST, self.guest_cid, 0)),
                        PendingWork::NONE,
                    )
                } else {
                    // Not ready yet, so wait for more data.
                    (
                        None,
                        PendingWork::rx(
                            conn.socket.await_read_ready(RxWork::PendingConnection(seq)),
                        ),
                    )
                }
            }
            RxWork::SendReset(key) => {
                // TODO: Check if the connection exists and remove it?
                (Some(new_rst_packet(self.guest_cid, key)), PendingWork::NONE)
            }
        }
    }

    fn get_next_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }
}

pub enum RxWork {
    Connection(ConnectionInstanceId),
    PendingConnection(u64),
    // For port combinations that may not actually exist
    SendReset(ConnKey),
}

fn new_reply_packet(key: ConnKey, op: Operation, guest_cid: u64, fwd_cnt: u32) -> VsockHeader {
    VsockHeader {
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
    }
}

fn new_rw_packet(key: ConnKey, guest_cid: u64, fwd_cnt: u32, len: u32) -> VsockHeader {
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
    }
}

fn new_shutdown_packet(
    key: ConnKey,
    guest_cid: u64,
    fwd_cnt: u32,
    flags: ShutdownFlags,
) -> VsockHeader {
    VsockHeader {
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
    }
}

fn new_rst_packet(guest_cid: u64, key: ConnKey) -> VsockHeader {
    VsockHeader {
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
    }
}
