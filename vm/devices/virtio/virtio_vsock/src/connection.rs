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

use crate::PendingWork;
use crate::RxWorkItem;
use crate::WriteReadyItem;
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
use futures::AsyncReadExt;
use futures::AsyncWriteExt;
use futures::io;
use pal_async::interest::PollEvents;
use pal_async::socket::PollReadyExt;
use pal_async::socket::PolledSocket;
use pal_async::socket::ReadHalf;
use pal_async::socket::WriteHalf;
use std::collections::HashMap;
use std::io::IoSlice;
use std::io::Write;
use std::num::Wrapping;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::path::PathBuf;
use unix_socket::UnixStream;
use vmcore::vm_task::VmTaskDriver;

const TX_BUF_SIZE: u32 = 65536;

/// A key that uniquely identifies a vsock connection.
/// TODO: I think these need a sequence number since some futures could outlive the connection.
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

#[bitfield(u32)]
struct PendingReply {
    reset: bool,
    respond: bool,
    #[bits(30)]
    _reserved: u32,
}

/// Tracks the state of a single vsock connection relayed to a Unix socket.
struct Connection {
    key: ConnKey,
    /// Buffer allocation advertised by the peer (guest).
    peer_buf_alloc: u32,
    /// Received data that the peer has forwarded from its buffer.
    peer_fwd_cnt: Wrapping<u32>,
    /// Data received from the peer that has been forwarded to the unix socket relay.
    fwd_cnt: Wrapping<u32>,
    last_sent_fwd_count: u32,
    pending_reply: PendingReply,
    socket: RelaySocket,
    recv_buf: Option<RingBuffer>,
    is_write_shutdown: bool,
}

impl Connection {
    fn new(
        key: ConnKey,
        peer_buf_alloc: u32,
        peer_fwd_cnt: Wrapping<u32>,
        pending_reply: PendingReply,
        socket: RelaySocket,
    ) -> Self {
        Self {
            key,
            peer_buf_alloc,
            peer_fwd_cnt,
            fwd_cnt: Wrapping(0),
            last_sent_fwd_count: 0,
            pending_reply,
            socket,
            recv_buf: None,
            is_write_shutdown: false,
        }
    }

    fn handle_guest_data(
        &mut self,
        data: &[IoSlice<'_>],
        data_len: usize,
    ) -> anyhow::Result<Option<WriteReadyItem>> {
        if self.is_write_shutdown {
            anyhow::bail!("peer has shutdown write side but sent data");
        }

        let bytes_sent = if self.recv_buf.as_ref().is_none_or(|buf| buf.is_empty()) {
            match self.socket.get().write_vectored(data) {
                Ok(n) => {
                    self.fwd_cnt += n as u32;
                    tracing::info!(self.fwd_cnt, self.last_sent_fwd_count, "forwarded");
                    if n == data_len {
                        return Ok(None);
                    }

                    n
                }
                Err(e) => {
                    if e.kind() != io::ErrorKind::WouldBlock {
                        return Err(e).context("failed to write to guest socket");
                    }

                    0
                }
            }
        } else {
            0
        };

        let buf = self
            .recv_buf
            .get_or_insert_with(|| RingBuffer::new(TX_BUF_SIZE as usize));

        let remaining = data_len - bytes_sent;

        // The guest should not do this since it knows how much space we have.
        if remaining > buf.available() {
            anyhow::bail!(
                "peer sent {} bytes, but only {} bytes available in buffer",
                remaining,
                buf.available()
            );
        }

        buf.write(data, bytes_sent);
        Ok(self.socket.await_write_ready(self.key))
    }

    fn write_from_buffer(&mut self) -> anyhow::Result<Option<WriteReadyItem>> {
        let buf = self.recv_buf.as_mut().expect("buffer must exist");
        match buf.read_to(&mut self.socket.get()) {
            Ok(_) => (),
            Err(e) => {
                if e.kind() != io::ErrorKind::WouldBlock {
                    return Err(e).context("failed to write buffered data to guest socket");
                }
            }
        }

        if buf.is_empty() {
            if self.is_write_shutdown {
                self.socket
                    .get()
                    .shutdown(std::net::Shutdown::Write)
                    .context("failed to shutdown write side of socket")?;
            }

            Ok(None)
        } else {
            Ok(self.socket.await_write_ready(self.key))
        }
    }

    fn peer_needs_credit_update(&self) -> bool {
        self.fwd_cnt.0 != self.last_sent_fwd_count
    }

    fn shutdown(&mut self, mut flags: ShutdownFlags) -> io::Result<()> {
        if flags.send() {
            // Don't actually shutdown the write side if we're still waiting to flush data out of
            // the buffer.
            if self.recv_buf.as_ref().is_some_and(|buf| !buf.is_empty()) {
                flags.set_send(false);
            }

            self.is_write_shutdown = true;
        }

        let how = if flags.send() {
            if flags.receive() {
                std::net::Shutdown::Both
            } else {
                std::net::Shutdown::Write
            }
        } else if flags.receive() {
            std::net::Shutdown::Read
        } else {
            return Ok(());
        };

        self.socket.get().shutdown(how)?;
        Ok(())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Shutdown the socket so any pending read/write polls will complete.
        let _ = self.socket.get().shutdown(std::net::Shutdown::Both);
    }
}

/// Messages sent from connection tasks back to the relay worker.
enum RelayEvent {
    /// Data received from the host-side Unix socket, to be forwarded to the
    /// guest via the rx queue.
    DataFromHost { key: ConnKey, data: Vec<u8> },
    /// The host-side connection has closed.
    HostClosed { key: ConnKey },
    /// A new connection request from a host listener (raw UnixStream).
    IncomingHostConnect { socket: UnixStream },
    /// A pre-built packet to send to the guest on the rx queue.
    SendPacket { hdr: VsockHeader, data: Vec<u8> },
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
}

impl ConnectionManager {
    /// Creates a new relay.
    ///
    /// `guest_cid` is the CID assigned to the guest.
    /// `base_path` is the directory path prefix for Unix sockets. For a vsock
    /// port P, the relay will try `<base_path>_P` first, then `<base_path>`.
    pub fn new(
        driver: VmTaskDriver,
        guest_cid: u64,
        base_path: PathBuf,
        listener: Option<UnixListener>,
    ) -> anyhow::Result<Self> {
        let relay = Self {
            guest_cid,
            relay: UnixSocketRelay::new(driver, base_path),
            conns: HashMap::new(),
        };

        Ok(relay)
    }

    /// Handle a packet received from the guest on the tx virtqueue.
    pub fn handle_guest_tx(&mut self, packet: VsockPacket<'_>) -> PendingWork {
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

            return PendingWork::rx(RxWork::SendReset(key));
        }

        let op = Operation(packet.header.op);
        match op {
            Operation::REQUEST => {
                tracing::info!(?packet.header, "connect request");
                // Guest is initiating a connection to a port on the host.
                match self.relay.connect(key.local_port) {
                    Ok(socket) => {
                        // TODO: Handle existing connection
                        self.conns.insert(
                            key,
                            Connection::new(
                                key,
                                packet.header.buf_alloc,
                                Wrapping(packet.header.fwd_cnt),
                                PendingReply::new().with_respond(true),
                                socket,
                            ),
                        );

                        PendingWork::rx(RxWork::Connection(key))
                    }
                    Err(err) => {
                        tracelimit::warn_ratelimited!(
                            error = err.as_ref() as &dyn std::error::Error,
                            port = key.local_port,
                            "failed to connect to host socket for vsock request"
                        );
                        PendingWork::rx(RxWork::SendReset(key))
                    }
                }
            }
            Operation::RW => {
                // Guest is sending data.
                let Some(conn) = self.conns.get_mut(&key) else {
                    tracelimit::warn_ratelimited!(?key, "RW for unknown connection");
                    return PendingWork::rx(RxWork::SendReset(key));
                };

                match conn.handle_guest_data(packet.data, packet.header.len as usize) {
                    Ok(future) => PendingWork::new(
                        future,
                        conn.peer_needs_credit_update()
                            .then_some(RxWork::Connection(key)),
                    ),
                    Err(err) => {
                        tracelimit::warn_ratelimited!(
                            error = err.as_ref() as &dyn std::error::Error,
                            ?key,
                            "failed to write guest data to host socket"
                        );
                        PendingWork::rx(RxWork::SendReset(key))
                    }
                }
                // if let Some(conn) = self.conns.get(&key) {
                //     if !data.is_empty() {
                //         conn.tx.send(data.to_vec());
                //     }
                // } else {
                //     tracing::debug!(?key, "RW for unknown connection, sending RST");
                //     return Some((
                //         VsockHeader::new_reply(
                //             dst_cid,
                //             src_cid,
                //             dst_port,
                //             src_port,
                //             protocol::VIRTIO_VSOCK_OP_RST,
                //         ),
                //         Vec::new(),
                //     ));
                // }
            }
            Operation::RESPONSE => {
                // Guest accepted a host-initiated connection.
                // Update credit info.
                // if let Some(conn) = self.conns.get_mut(&key) {
                //     conn.peer_buf_alloc = u32::from_le(hdr.buf_alloc);
                //     conn.peer_fwd_cnt = u32::from_le(hdr.fwd_cnt);
                // }
                todo!();
            }
            Operation::SHUTDOWN => {
                let Some(conn) = self.conns.get_mut(&key) else {
                    tracelimit::warn_ratelimited!(?key, "SHUTDOWN for unknown connection");
                    return PendingWork::rx(RxWork::SendReset(key));
                };

                if let Err(err) = conn.shutdown(ShutdownFlags::from_bits(packet.header.flags)) {
                    tracelimit::warn_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        ?key,
                        "failed to shutdown connection"
                    );

                    PendingWork::rx(RxWork::SendReset(key))
                } else {
                    PendingWork::NONE
                }
            }
            Operation::RST => {
                if let Some(_conn) = self.conns.remove(&key) {
                    tracing::debug!(?key, "guest reset connection");
                }
                PendingWork::NONE
            }
            Operation::CREDIT_UPDATE => {
                // let mut conns = self.conns.lock();
                // if let Some(conn) = conns.get_mut(&key) {
                //     conn.peer_buf_alloc = u32::from_le(hdr.buf_alloc);
                //     conn.peer_fwd_cnt = u32::from_le(hdr.fwd_cnt);
                // }
                // None
                todo!();
            }
            Operation::CREDIT_REQUEST => {
                // Guest is requesting credit info from us. Reply with a
                // CREDIT_UPDATE.
                // Some((
                //     VsockHeader::new_reply(
                //         dst_cid,
                //         src_cid,
                //         dst_port,
                //         src_port,
                //         protocol::VIRTIO_VSOCK_OP_CREDIT_UPDATE,
                //     ),
                //     Vec::new(),
                // ))
                todo!();
            }
            _ => {
                tracing::debug!(header = ?packet.header, "unknown vsock operation");
                // TODO: Send RST for unknown operations?
                PendingWork::NONE
            }
        }
    }

    pub fn handle_write_ready(&mut self, key: ConnKey) -> PendingWork {
        let Some(conn) = self.conns.get_mut(&key) else {
            // This is fine if the connection was reset but a write future was still pending.
            tracing::debug!(?key, "write ready for unknown connection");
            return PendingWork::NONE;
        };

        match conn.write_from_buffer() {
            Ok(future) => PendingWork::new(
                future,
                conn.peer_needs_credit_update()
                    .then_some(RxWork::Connection(key)),
            ),
            Err(err) => {
                tracelimit::warn_ratelimited!(
                    error = err.as_ref() as &dyn std::error::Error,
                    ?key,
                    "failed to write buffered data to host socket on write ready"
                );
                PendingWork::rx(RxWork::SendReset(key))
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

    pub fn get_rx_packet(&mut self, work: RxWork) -> Option<VsockHeader> {
        match work {
            RxWork::Connection(key) => {
                let conn = self.conns.get_mut(&key)?;
                if conn.pending_reply.reset() {
                    // Remove the connection immediately on reset.
                    self.conns.remove(&key);
                    Some(self.new_rst_packet(key))
                } else if conn.pending_reply.respond() {
                    conn.pending_reply.set_respond(false);
                    conn.last_sent_fwd_count = conn.fwd_cnt.0;
                    let fwd_cnt = conn.fwd_cnt.0;

                    Some(self.new_reply_packet(key, Operation::RESPONSE, fwd_cnt))
                } else if conn.peer_needs_credit_update() {
                    conn.last_sent_fwd_count = conn.fwd_cnt.0;
                    let fwd_cnt = conn.fwd_cnt.0;

                    tracing::info!(?key, fwd_cnt, "sending credit update");
                    Some(self.new_reply_packet(key, Operation::CREDIT_UPDATE, fwd_cnt))
                } else {
                    assert_eq!(conn.pending_reply.into_bits(), 0);
                    None
                }

                // TODO: Check for socket data
            }
            RxWork::SendReset(key) => {
                // TODO: Check if the connection exists and remove it?
                Some(self.new_rst_packet(key))
            }
        }
    }

    /// Process relay events and return pending rx packets for the guest.
    ///
    /// This should be called regularly from the device worker to collect data
    /// that needs to be sent to the guest.
    pub fn poll_rx_packets(&mut self) -> Vec<(VsockHeader, Vec<u8>)> {
        Vec::new() // TODO
        // let mut packets = Vec::new();

        // // Collect all pending events first to avoid borrow conflicts.
        // let events: Vec<_> = {
        //     let recv = match &mut self.event_recv {
        //         Some(r) => r,
        //         None => return packets,
        //     };
        //     let mut events = Vec::new();
        //     while let Ok(event) = recv.try_recv() {
        //         events.push(event);
        //     }
        //     events
        // };

        // for event in events {
        //     match event {
        //         RelayEvent::DataFromHost { key, data } => {
        //             let mut hdr = VsockHeader::new_reply(
        //                 VSOCK_CID_HOST,
        //                 key.peer_cid,
        //                 key.local_port,
        //                 key.peer_port,
        //                 protocol::VIRTIO_VSOCK_OP_RW,
        //             );
        //             hdr.len = (data.len() as u32).to_le();
        //             packets.push((hdr, data));
        //         }
        //         RelayEvent::HostClosed { key } => {
        //             let mut conns = self.conns.lock();
        //             if conns.remove(&key).is_some() {
        //                 // Send SHUTDOWN to the guest.
        //                 let mut shutdown_hdr = VsockHeader::new_reply(
        //                     VSOCK_CID_HOST,
        //                     key.peer_cid,
        //                     key.local_port,
        //                     key.peer_port,
        //                     protocol::VIRTIO_VSOCK_OP_SHUTDOWN,
        //                 );
        //                 shutdown_hdr.flags = (protocol::VIRTIO_VSOCK_SHUTDOWN_F_RECEIVE
        //                     | protocol::VIRTIO_VSOCK_SHUTDOWN_F_SEND)
        //                     .to_le();
        //                 packets.push((shutdown_hdr, Vec::new()));
        //             }
        //         }
        //         RelayEvent::SendPacket { hdr, data } => {
        //             packets.push((hdr, data));
        //         }
        //         RelayEvent::IncomingHostConnect { socket } => {
        //             // Host wants to connect to a guest port. We need to read
        //             // the CONNECT request to determine the target port.
        //             let local_port = self.alloc_local_port();

        //             let (tx_send, tx_recv) = mesh::channel();
        //             let conn = Connection {
        //                 tx: tx_send,
        //                 peer_buf_alloc: 0,
        //                 peer_fwd_cnt: 0,
        //             };

        //             // Store with port 0 initially; will be updated async.
        //             let placeholder_key = ConnKey {
        //                 local_port,
        //                 peer_port: 0,
        //                 peer_cid: self.guest_cid,
        //             };
        //             self.conns.lock().insert(placeholder_key, conn);

        //             let driver = self.driver.clone();
        //             let event_send = self.event_send.clone();
        //             let guest_cid = self.guest_cid;

        //             self.tasks.push(self.driver.spawn(
        //                 format!("vsock-host-relay-{local_port}"),
        //                 async move {
        //                     let mut polled = match PolledSocket::new(&driver, socket) {
        //                         Ok(s) => s,
        //                         Err(err) => {
        //                             tracing::debug!(
        //                                 error = &err as &dyn std::error::Error,
        //                                 "failed to create polled socket for host connect"
        //                             );
        //                             return;
        //                         }
        //                     };
        //                     let port = match read_vsock_connect(&mut polled).await {
        //                         Ok(p) => p,
        //                         Err(err) => {
        //                             tracing::debug!(
        //                                 error = err.as_ref() as &dyn std::error::Error,
        //                                 "failed to read vsock connect request"
        //                             );
        //                             return;
        //                         }
        //                     };

        //                     let key = ConnKey {
        //                         local_port,
        //                         peer_port: port,
        //                         peer_cid: guest_cid,
        //                     };

        //                     // Send the REQUEST to the guest.
        //                     let hdr = VsockHeader::new_reply(
        //                         VSOCK_CID_HOST,
        //                         guest_cid,
        //                         local_port,
        //                         port,
        //                         protocol::VIRTIO_VSOCK_OP_REQUEST,
        //                     );
        //                     event_send.send(RelayEvent::SendPacket {
        //                         hdr,
        //                         data: Vec::new(),
        //                     });

        //                     // Send OK to the host client.
        //                     let ok_msg = format!("OK {}\n", local_port);
        //                     if let Err(err) = polled.write_all(ok_msg.as_bytes()).await {
        //                         tracing::debug!(
        //                             error = &err as &dyn std::error::Error,
        //                             "failed to write OK to host"
        //                         );
        //                         return;
        //                     }

        //                     if let Err(err) =
        //                         relay_data(&driver, key, polled, tx_recv, &event_send).await
        //                     {
        //                         tracing::debug!(
        //                             error = err.as_ref() as &dyn std::error::Error,
        //                             "host-initiated vsock relay failed"
        //                         );
        //                     }
        //                     event_send.send(RelayEvent::HostClosed { key });
        //                 },
        //             ));
        //         }
        //     }
        // }
        // packets
    }

    fn new_reply_packet(&self, key: ConnKey, op: Operation, fwd_cnt: u32) -> VsockHeader {
        VsockHeader {
            src_cid: VSOCK_CID_HOST,
            dst_cid: self.guest_cid,
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

    fn new_rst_packet(&self, key: ConnKey) -> VsockHeader {
        VsockHeader {
            src_cid: VSOCK_CID_HOST,
            dst_cid: self.guest_cid,
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
}

/// Read a hybrid vsock connect request (`CONNECT <port>\n`) from a Unix socket.
async fn read_vsock_connect(socket: &mut PolledSocket<UnixStream>) -> anyhow::Result<u32> {
    let mut buf = [0u8; 64];
    let mut i = 0;
    while i == 0 || buf[i - 1] != b'\n' {
        if i == buf.len() {
            anyhow::bail!("vsock connect request too long");
        }
        let n = socket
            .read(&mut buf[i..])
            .await
            .context("failed to read connect request")?;
        if n == 0 {
            anyhow::bail!("connection closed before connect request completed");
        }
        i += n;
    }

    let line = &buf[..i - 1]; // strip newline
    let rest = line
        .strip_prefix(b"CONNECT ")
        .context("invalid connect request: missing CONNECT prefix")?;
    let port_str = std::str::from_utf8(rest).context("invalid connect request: not UTF-8")?;
    let port: u32 = port_str
        .parse()
        .context("invalid connect request: bad port number")?;
    Ok(port)
}

/// Run a relay for a guest-initiated connection to a host Unix socket.
async fn run_connection_relay(
    driver: &VmTaskDriver,
    key: ConnKey,
    path: &Path,
    tx_recv: mesh::Receiver<Vec<u8>>,
    event_send: &mesh::Sender<RelayEvent>,
) -> anyhow::Result<()> {
    let socket = PolledSocket::connect_unix(driver, path)
        .await
        .with_context(|| {
            format!(
                "failed to connect to {} for vsock port {}",
                path.display(),
                key.local_port,
            )
        })?;

    tracing::debug!(
        port = key.local_port,
        path = %path.display(),
        "connected to host Unix socket for vsock relay"
    );

    relay_data(driver, key, socket, tx_recv, event_send).await
}

/// Bidirectional relay between a Unix socket and the vsock connection.
async fn relay_data(
    _driver: &VmTaskDriver,
    key: ConnKey,
    socket: PolledSocket<UnixStream>,
    mut tx_recv: mesh::Receiver<Vec<u8>>,
    event_send: &mesh::Sender<RelayEvent>,
) -> anyhow::Result<()> {
    let (mut sock_read, mut sock_write) = socket.split();

    // Socket -> guest: read from Unix socket, send to relay event channel.
    let socket_to_guest = async {
        let mut buf = vec![0u8; 4096];
        loop {
            let n = sock_read.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            event_send.send(RelayEvent::DataFromHost {
                key,
                data: buf[..n].to_vec(),
            });
        }
        Ok::<_, std::io::Error>(())
    };

    // Guest -> socket: receive data from mesh channel, write to Unix socket.
    let guest_to_socket = async {
        while let Ok(data) = tx_recv.recv().await {
            if data.is_empty() {
                break;
            }
            sock_write.write_all(&data).await?;
        }
        sock_write.close().await?;
        Ok::<_, std::io::Error>(())
    };

    let result = futures::future::try_join(socket_to_guest, guest_to_socket).await;
    match result {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::ConnectionReset => Ok(()),
        Err(err) => Err(err.into()),
    }
}

pub enum RxWork {
    Connection(ConnKey),
    // For port combinations that may not actually exist
    SendReset(ConnKey),
}
