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

use crate::RxWorkQueue;
use crate::spec::Operation;
use crate::spec::ShutdownFlags;
use crate::spec::SocketType;
use crate::spec::VSOCK_CID_HOST;
use crate::spec::VsockHeader;
use crate::spec::VsockPacket;
use crate::unix_relay::UnixSocketRelay;
use anyhow::Context;
use bitfield_struct::bitfield;
use futures::AsyncReadExt;
use futures::AsyncWriteExt;
use pal_async::socket::PolledSocket;
use std::collections::HashMap;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::path::PathBuf;
use unix_socket::UnixStream;
use vmcore::vm_task::VmTaskDriver;

const TX_BUF_SIZE: u32 = 65536;

/// A key that uniquely identifies a vsock connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnKey {
    local_port: u32,
    peer_port: u32,
}

impl ConnKey {
    fn from_tx_packet(hdr: &VsockHeader) -> Self {
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
    /// Total bytes received by the peer (guest) so far.
    peer_fwd_cnt: u32,
    pending_reply: PendingReply,
    // TODO: I think we need to split this, and then give ownership of the read half to the future
    // added to the work queue. There also needs to be a way to terminate the future when the,
    // connection is closed or reset, e.g. an Arc<AtomicBool> or something.
    socket: PolledSocket<UnixStream>,
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
    pub fn handle_guest_tx(&mut self, packet: VsockPacket<'_>) -> Option<RxWork> {
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

            return Some(RxWork::SendReset(key));
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
                            Connection {
                                key,
                                peer_buf_alloc: packet.header.buf_alloc,
                                peer_fwd_cnt: packet.header.fwd_cnt,
                                pending_reply: PendingReply::new().with_respond(true),
                                socket,
                            },
                        );

                        Some(RxWork::Connection(key))
                    }
                    Err(err) => {
                        tracelimit::warn_ratelimited!(
                            error = err.as_ref() as &dyn std::error::Error,
                            port = key.local_port,
                            "failed to connect to host socket for vsock request"
                        );
                        Some(RxWork::SendReset(key))
                    }
                }
            }
            Operation::RW => {
                // Guest is sending data.
                tracing::info!(?packet.header, "We got data!");
                None
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
                // let mut conns = self.conns.lock();
                // if let Some(conn) = conns.remove(&key) {
                //     drop(conn);
                //     tracing::debug!(?key, flags, "guest shutdown connection");
                // }
                // // Send RST to complete the shutdown.
                // Some((
                //     VsockHeader::new_reply(
                //         dst_cid,
                //         src_cid,
                //         dst_port,
                //         src_port,
                //         protocol::VIRTIO_VSOCK_OP_RST,
                //     ),
                //     Vec::new(),
                // ))
                todo!();
            }
            Operation::RST => {
                if let Some(conn) = self.conns.remove(&key) {
                    tracing::debug!(?key, "guest reset connection");
                }
                None
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
                None
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

    pub fn get_rx_packet(&mut self, work: RxWork) -> Option<VsockPacket<'_>> {
        match work {
            RxWork::Connection(key) => {
                let conn = self.conns.get_mut(&key)?;
                if conn.pending_reply.reset() {
                    // Remove the connection immediately on reset.
                    self.conns.remove(&key);
                    Some(self.new_rst_packet(key))
                } else if conn.pending_reply.respond() {
                    conn.pending_reply.set_respond(false);

                    Some(self.new_reply_packet(key, Operation::RESPONSE))
                } else {
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

    fn new_reply_packet(&self, key: ConnKey, op: Operation) -> VsockPacket<'_> {
        VsockPacket::header_only(VsockHeader {
            src_cid: VSOCK_CID_HOST,
            dst_cid: self.guest_cid,
            src_port: key.local_port,
            dst_port: key.peer_port,
            len: 0,
            socket_type: SocketType::STREAM.0,
            op: op.0,
            flags: ShutdownFlags::new().into(),
            buf_alloc: TX_BUF_SIZE,
            fwd_cnt: 0, // TODO!
        })
    }

    fn new_rst_packet(&self, key: ConnKey) -> VsockPacket<'_> {
        VsockPacket::header_only(VsockHeader {
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
        })
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
