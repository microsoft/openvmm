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

use crate::protocol;
use crate::protocol::VSOCK_CID_HOST;
use crate::protocol::VsockHeader;
use anyhow::Context;
use futures::AsyncReadExt;
use futures::AsyncWriteExt;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pal_async::task::Task;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use unix_socket::UnixListener;
use unix_socket::UnixStream;
use vmcore::vm_task::VmTaskDriver;

/// A key that uniquely identifies a vsock connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ConnKey {
    local_port: u32,
    peer_port: u32,
    peer_cid: u64,
}

/// Tracks the state of a single vsock connection relayed to a Unix socket.
struct Connection {
    /// Sender for data from the guest to the Unix socket.
    tx: mesh::Sender<Vec<u8>>,
    /// Buffer allocation advertised by the peer (guest).
    peer_buf_alloc: u32,
    /// Total bytes received by the peer (guest) so far.
    peer_fwd_cnt: u32,
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
pub struct VsockRelay {
    guest_cid: u64,
    driver: VmTaskDriver,
    base_path: PathBuf,
    conns: Arc<Mutex<HashMap<ConnKey, Connection>>>,
    event_send: mesh::Sender<RelayEvent>,
    event_recv: Option<mesh::Receiver<RelayEvent>>,
    tasks: Vec<Task<()>>,
    next_local_port: u32,
}

impl VsockRelay {
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
        let (event_send, event_recv) = mesh::channel();

        let mut relay = Self {
            guest_cid,
            driver: driver.clone(),
            base_path,
            conns: Arc::new(Mutex::new(HashMap::new())),
            event_send: event_send.clone(),
            event_recv: Some(event_recv),
            tasks: Vec::new(),
            next_local_port: 1024,
        };

        if let Some(listener) = listener {
            let polled = PolledSocket::new(&driver, listener)?;
            let send = event_send;
            relay
                .tasks
                .push(driver.spawn("vsock-listener", Self::run_listener(polled, send)));
        }

        Ok(relay)
    }

    async fn run_listener(
        mut listener: PolledSocket<UnixListener>,
        send: mesh::Sender<RelayEvent>,
    ) {
        loop {
            let (connection, _addr) = match listener.accept().await {
                Ok(c) => c,
                Err(err) => {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "vsock listener accept failed, shutting down"
                    );
                    break;
                }
            };

            let send = send.clone();
            // Send the raw stream for connect request processing.
            send.send(RelayEvent::IncomingHostConnect { socket: connection });
        }
    }

    /// Allocate a local port number for a new connection.
    fn alloc_local_port(&mut self) -> u32 {
        let port = self.next_local_port;
        self.next_local_port = self.next_local_port.wrapping_add(1).max(1024);
        port
    }

    /// Handle a packet received from the guest on the tx virtqueue.
    ///
    /// Returns an optional response header + data to place on the rx virtqueue.
    pub fn handle_guest_tx(
        &mut self,
        hdr: &VsockHeader,
        data: &[u8],
    ) -> Option<(VsockHeader, Vec<u8>)> {
        let src_cid = u64::from_le(hdr.src_cid);
        let dst_cid = u64::from_le(hdr.dst_cid);
        let src_port = u32::from_le(hdr.src_port);
        let dst_port = u32::from_le(hdr.dst_port);
        let op = u16::from_le(hdr.op);
        let pkt_type = u16::from_le(hdr.socket_type);
        let flags = u32::from_le(hdr.flags);

        if pkt_type != protocol::VIRTIO_VSOCK_TYPE_STREAM {
            tracing::debug!(pkt_type, "ignoring non-stream vsock packet");
            return None;
        }

        // Verify the source CID matches the guest.
        if src_cid != self.guest_cid {
            tracing::debug!(
                src_cid,
                guest_cid = self.guest_cid,
                "ignoring packet with wrong source CID"
            );
            return None;
        }

        let key = ConnKey {
            local_port: dst_port,
            peer_port: src_port,
            peer_cid: src_cid,
        };

        match op {
            protocol::VIRTIO_VSOCK_OP_REQUEST => {
                // Guest is initiating a connection to a port on the host.
                self.handle_connect_request(key, src_cid, dst_cid, src_port, dst_port, hdr)
            }
            protocol::VIRTIO_VSOCK_OP_RW => {
                // Guest is sending data.
                let conns = self.conns.lock();
                if let Some(conn) = conns.get(&key) {
                    if !data.is_empty() {
                        conn.tx.send(data.to_vec());
                    }
                } else {
                    tracing::debug!(?key, "RW for unknown connection, sending RST");
                    return Some((
                        VsockHeader::new_reply(
                            dst_cid,
                            src_cid,
                            dst_port,
                            src_port,
                            protocol::VIRTIO_VSOCK_OP_RST,
                        ),
                        Vec::new(),
                    ));
                }
                None
            }
            protocol::VIRTIO_VSOCK_OP_RESPONSE => {
                // Guest accepted a host-initiated connection.
                // Update credit info.
                let mut conns = self.conns.lock();
                if let Some(conn) = conns.get_mut(&key) {
                    conn.peer_buf_alloc = u32::from_le(hdr.buf_alloc);
                    conn.peer_fwd_cnt = u32::from_le(hdr.fwd_cnt);
                }
                None
            }
            protocol::VIRTIO_VSOCK_OP_SHUTDOWN => {
                let mut conns = self.conns.lock();
                if let Some(conn) = conns.remove(&key) {
                    drop(conn);
                    tracing::debug!(?key, flags, "guest shutdown connection");
                }
                // Send RST to complete the shutdown.
                Some((
                    VsockHeader::new_reply(
                        dst_cid,
                        src_cid,
                        dst_port,
                        src_port,
                        protocol::VIRTIO_VSOCK_OP_RST,
                    ),
                    Vec::new(),
                ))
            }
            protocol::VIRTIO_VSOCK_OP_RST => {
                let mut conns = self.conns.lock();
                if let Some(conn) = conns.remove(&key) {
                    drop(conn);
                    tracing::debug!(?key, "guest reset connection");
                }
                None
            }
            protocol::VIRTIO_VSOCK_OP_CREDIT_UPDATE => {
                let mut conns = self.conns.lock();
                if let Some(conn) = conns.get_mut(&key) {
                    conn.peer_buf_alloc = u32::from_le(hdr.buf_alloc);
                    conn.peer_fwd_cnt = u32::from_le(hdr.fwd_cnt);
                }
                None
            }
            protocol::VIRTIO_VSOCK_OP_CREDIT_REQUEST => {
                // Guest is requesting credit info from us. Reply with a
                // CREDIT_UPDATE.
                Some((
                    VsockHeader::new_reply(
                        dst_cid,
                        src_cid,
                        dst_port,
                        src_port,
                        protocol::VIRTIO_VSOCK_OP_CREDIT_UPDATE,
                    ),
                    Vec::new(),
                ))
            }
            _ => {
                tracing::debug!(op, "unknown vsock operation");
                None
            }
        }
    }

    fn handle_connect_request(
        &mut self,
        key: ConnKey,
        src_cid: u64,
        dst_cid: u64,
        src_port: u32,
        dst_port: u32,
        hdr: &VsockHeader,
    ) -> Option<(VsockHeader, Vec<u8>)> {
        // Check if we can connect to a Unix socket for this port.
        let path = match self.host_uds_path(dst_port) {
            Ok(p) => p,
            Err(err) => {
                tracelimit::warn_ratelimited!(
                    error = err.as_ref() as &dyn std::error::Error,
                    dst_port,
                    "no host socket for vsock port"
                );
                return Some((
                    VsockHeader::new_reply(
                        dst_cid,
                        src_cid,
                        dst_port,
                        src_port,
                        protocol::VIRTIO_VSOCK_OP_RST,
                    ),
                    Vec::new(),
                ));
            }
        };

        // Spawn a task to connect and relay data.
        let (tx_send, tx_recv) = mesh::channel();
        let conn = Connection {
            tx: tx_send,
            peer_buf_alloc: u32::from_le(hdr.buf_alloc),
            peer_fwd_cnt: u32::from_le(hdr.fwd_cnt),
        };
        self.conns.lock().insert(key, conn);

        let driver = self.driver.clone();
        let event_send = self.event_send.clone();

        self.tasks.push(
            self.driver
                .spawn(format!("vsock-relay-{dst_port}"), async move {
                    if let Err(err) =
                        run_connection_relay(&driver, key, &path, tx_recv, &event_send).await
                    {
                        tracing::debug!(
                            error = err.as_ref() as &dyn std::error::Error,
                            dst_port,
                            "vsock connection relay failed"
                        );
                    }
                    event_send.send(RelayEvent::HostClosed { key });
                }),
        );

        // Send RESPONSE to accept the connection.
        Some((
            VsockHeader::new_reply(
                dst_cid,
                src_cid,
                dst_port,
                src_port,
                protocol::VIRTIO_VSOCK_OP_RESPONSE,
            ),
            Vec::new(),
        ))
    }

    /// Process relay events and return pending rx packets for the guest.
    ///
    /// This should be called regularly from the device worker to collect data
    /// that needs to be sent to the guest.
    pub fn poll_rx_packets(&mut self) -> Vec<(VsockHeader, Vec<u8>)> {
        let mut packets = Vec::new();

        // Collect all pending events first to avoid borrow conflicts.
        let events: Vec<_> = {
            let recv = match &mut self.event_recv {
                Some(r) => r,
                None => return packets,
            };
            let mut events = Vec::new();
            while let Ok(event) = recv.try_recv() {
                events.push(event);
            }
            events
        };

        for event in events {
            match event {
                RelayEvent::DataFromHost { key, data } => {
                    let mut hdr = VsockHeader::new_reply(
                        VSOCK_CID_HOST,
                        key.peer_cid,
                        key.local_port,
                        key.peer_port,
                        protocol::VIRTIO_VSOCK_OP_RW,
                    );
                    hdr.len = (data.len() as u32).to_le();
                    packets.push((hdr, data));
                }
                RelayEvent::HostClosed { key } => {
                    let mut conns = self.conns.lock();
                    if conns.remove(&key).is_some() {
                        // Send SHUTDOWN to the guest.
                        let mut shutdown_hdr = VsockHeader::new_reply(
                            VSOCK_CID_HOST,
                            key.peer_cid,
                            key.local_port,
                            key.peer_port,
                            protocol::VIRTIO_VSOCK_OP_SHUTDOWN,
                        );
                        shutdown_hdr.flags = (protocol::VIRTIO_VSOCK_SHUTDOWN_F_RECEIVE
                            | protocol::VIRTIO_VSOCK_SHUTDOWN_F_SEND)
                            .to_le();
                        packets.push((shutdown_hdr, Vec::new()));
                    }
                }
                RelayEvent::SendPacket { hdr, data } => {
                    packets.push((hdr, data));
                }
                RelayEvent::IncomingHostConnect { socket } => {
                    // Host wants to connect to a guest port. We need to read
                    // the CONNECT request to determine the target port.
                    let local_port = self.alloc_local_port();

                    let (tx_send, tx_recv) = mesh::channel();
                    let conn = Connection {
                        tx: tx_send,
                        peer_buf_alloc: 0,
                        peer_fwd_cnt: 0,
                    };

                    // Store with port 0 initially; will be updated async.
                    let placeholder_key = ConnKey {
                        local_port,
                        peer_port: 0,
                        peer_cid: self.guest_cid,
                    };
                    self.conns.lock().insert(placeholder_key, conn);

                    let driver = self.driver.clone();
                    let event_send = self.event_send.clone();
                    let guest_cid = self.guest_cid;

                    self.tasks.push(self.driver.spawn(
                        format!("vsock-host-relay-{local_port}"),
                        async move {
                            let mut polled = match PolledSocket::new(&driver, socket) {
                                Ok(s) => s,
                                Err(err) => {
                                    tracing::debug!(
                                        error = &err as &dyn std::error::Error,
                                        "failed to create polled socket for host connect"
                                    );
                                    return;
                                }
                            };
                            let port = match read_vsock_connect(&mut polled).await {
                                Ok(p) => p,
                                Err(err) => {
                                    tracing::debug!(
                                        error = err.as_ref() as &dyn std::error::Error,
                                        "failed to read vsock connect request"
                                    );
                                    return;
                                }
                            };

                            let key = ConnKey {
                                local_port,
                                peer_port: port,
                                peer_cid: guest_cid,
                            };

                            // Send the REQUEST to the guest.
                            let hdr = VsockHeader::new_reply(
                                VSOCK_CID_HOST,
                                guest_cid,
                                local_port,
                                port,
                                protocol::VIRTIO_VSOCK_OP_REQUEST,
                            );
                            event_send.send(RelayEvent::SendPacket {
                                hdr,
                                data: Vec::new(),
                            });

                            // Send OK to the host client.
                            let ok_msg = format!("OK {}\n", local_port);
                            if let Err(err) = polled.write_all(ok_msg.as_bytes()).await {
                                tracing::debug!(
                                    error = &err as &dyn std::error::Error,
                                    "failed to write OK to host"
                                );
                                return;
                            }

                            if let Err(err) =
                                relay_data(&driver, key, polled, tx_recv, &event_send).await
                            {
                                tracing::debug!(
                                    error = err.as_ref() as &dyn std::error::Error,
                                    "host-initiated vsock relay failed"
                                );
                            }
                            event_send.send(RelayEvent::HostClosed { key });
                        },
                    ));
                }
            }
        }
        packets
    }

    /// Look up the Unix domain socket path for a given vsock port.
    fn host_uds_path(&self, port: u32) -> anyhow::Result<PathBuf> {
        // Try port-specific path first: <base_path>_<port>
        let mut path = self.base_path.as_os_str().to_owned();
        path.push(format!("_{port}"));
        let specific = PathBuf::from(&path);
        if specific.try_exists()? {
            return Ok(specific);
        }

        // Fall back to the base path itself.
        if self.base_path.try_exists()? {
            return Ok(self.base_path.clone());
        }

        anyhow::bail!(
            "no vsock listener at {} for port {port}",
            self.base_path.display()
        );
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
