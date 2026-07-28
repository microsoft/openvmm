// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod assembler;
mod ring;

use super::Access;
use super::BindError;
use super::Client;
use super::DropReason;
use crate::ChecksumState;
use crate::ConsommeState;
use crate::FourTuple;
use crate::IpAddresses;
use crate::IpVersion;
use crate::PortForwardKey;
use crate::dns_resolver::DnsResolver;
use crate::dns_resolver::dns_tcp::DnsTcpHandler;
use futures::AsyncRead;
use futures::AsyncWrite;
use inspect::Inspect;
use inspect::InspectMut;
use inspect_counters::Counter;
use inspect_counters::Histogram;
use pal_async::driver::Driver;
use pal_async::interest::PollEvents;
use pal_async::socket::PollReady;
use pal_async::socket::PolledSocket;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::ETHERNET_HEADER_LEN;
use smoltcp::wire::EthernetFrame;
use smoltcp::wire::EthernetProtocol;
use smoltcp::wire::IPV4_HEADER_LEN;
use smoltcp::wire::IPV6_HEADER_LEN;
use smoltcp::wire::IpAddress;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::IpRepr;
use smoltcp::wire::Ipv4Packet;
use smoltcp::wire::Ipv6Packet;
use smoltcp::wire::TcpControl;
use smoltcp::wire::TcpPacket;
use smoltcp::wire::TcpRepr;
use smoltcp::wire::TcpSeqNumber;
use socket2::Domain;
use socket2::Protocol;
use socket2::SockAddr;
use socket2::Socket;
use socket2::Type;
use std::collections::HashMap;
use std::collections::hash_map;
use std::io;
use std::io::ErrorKind;
use std::io::IoSlice;
use std::io::IoSliceMut;
use std::net::IpAddr;
use std::net::Shutdown;
use std::net::SocketAddr;
use std::net::SocketAddrV4;
use std::net::SocketAddrV6;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use thiserror::Error;

#[derive(InspectMut)]
pub(crate) struct Tcp {
    #[inspect(iter_by_key)]
    connections: HashMap<FourTuple, TcpConnection>,
    #[inspect(iter_by_key)]
    listeners: HashMap<PortForwardKey, TcpListener>,
    connection_params: ConnectionParams,
    aggregate_stats: TcpAggregateStats,
}

/// Aggregate statistics across all TCP connections for inspect/diagnostics.
#[derive(Inspect, Default)]
struct TcpAggregateStats {
    connections_accepted: Counter,
    connections_initiated: Counter,
    /// Connections closed normally (LastAck final ACK, TimeWait, FIN exchange).
    connections_closed_normal: Counter,
    /// Connections closed by receiving a valid RST from the peer.
    connections_closed_peer_rst: Counter,
    /// Connections closed due to local errors (socket failures, invalid handshake).
    connections_closed_local_error: Counter,
}

impl TcpAggregateStats {
    fn record_close(&mut self, reason: ConnectionCloseReason) {
        match reason {
            ConnectionCloseReason::Normal => self.connections_closed_normal.increment(),
            ConnectionCloseReason::PeerRst => self.connections_closed_peer_rst.increment(),
            ConnectionCloseReason::LocalError => self.connections_closed_local_error.increment(),
        }
    }
}

#[derive(Inspect)]
struct ConnectionParams {
    rx_buffer: NormalizedBufferBounds,
    tx_buffer: NormalizedBufferBounds,
}

/// Normalized version of [`crate::TcpBufferBounds`] with both values clamped
/// to `[16 KiB, 4 MiB]` and rounded up to a power of two, then `initial`
/// further clamped to be no greater than `max`.
#[derive(Inspect, Clone, Copy, Debug)]
struct NormalizedBufferBounds {
    initial: usize,
    max: usize,
}

impl NormalizedBufferBounds {
    fn from_bounds(b: crate::TcpBufferBounds) -> Self {
        let clamp = |v: usize| v.clamp(16 << 10, 4 << 20).next_power_of_two();
        let max = clamp(b.max);
        let initial = clamp(b.initial).min(max);
        Self { initial, max }
    }
}

#[derive(Debug, Error)]
pub enum TcpError {
    #[error("still connecting")]
    StillConnecting,
    #[error("unacceptable segment number")]
    Unacceptable,
    #[error("missing ack bit")]
    MissingAck,
    #[error("ack newer than sequence")]
    AckPastSequence,
    #[error("invalid window scale")]
    InvalidWindowScale,
}

impl Tcp {
    pub fn new(rx_buffer: crate::TcpBufferBounds, tx_buffer: crate::TcpBufferBounds) -> Self {
        Self {
            connections: HashMap::new(),
            listeners: HashMap::new(),
            connection_params: ConnectionParams {
                rx_buffer: NormalizedBufferBounds::from_bounds(rx_buffer),
                tx_buffer: NormalizedBufferBounds::from_bounds(tx_buffer),
            },
            aggregate_stats: TcpAggregateStats::default(),
        }
    }
}

#[derive(Inspect)]
#[inspect(tag = "info")]
enum LoopbackPortInfo {
    None,
    ProxyForGuestPort { sending_port: u16, guest_port: u16 },
}

/// The I/O backend for a TCP connection.
///
/// A connection is either backed by a real host socket or a virtual DNS
/// handler that resolves DNS queries without a real socket.
enum TcpBackend {
    /// A real host socket. The socket may be `None` while the connection is
    /// being constructed, or after both ends have closed.
    Socket(Option<PolledSocket<Socket>>),
    /// A virtual DNS TCP handler (no real socket).
    Dns(DnsTcpHandler),
}

#[derive(Inspect)]
struct TcpConnection {
    #[inspect(skip)]
    backend: TcpBackend,
    #[inspect(flatten)]
    inner: TcpConnectionInner,
}

#[derive(Inspect)]
struct TcpConnectionInner {
    loopback_port: LoopbackPortInfo,
    state: TcpState,

    #[inspect(with = "|x| x.len()")]
    rx_buffer: ring::Ring,
    #[inspect(hex)]
    rx_window_cap: usize,
    rx_window_scale: u8,
    /// Autotune ceiling for `rx_window_cap`. Once `rx_window_cap` reaches this
    /// value, no further grow is attempted. The backing ring is rounded up to a
    /// power of two, so its allocated capacity can slightly exceed this value
    /// (e.g. when window scaling is disabled and this is capped to `u16::MAX`,
    /// the ring is 65536 while this is 65535).
    #[inspect(hex)]
    rx_buffer_max: usize,
    #[inspect(with = "inspect_seq")]
    rx_seq: TcpSeqNumber,
    /// Window last advertised to the guest, as it reconstructs it after
    /// window-scale truncation. Used to detect a zero-window reopen.
    #[inspect(hex)]
    rx_window_last_adv: usize,
    #[inspect(flatten)]
    rx_assembler: assembler::Assembler,
    needs_ack: bool,
    is_shutdown: bool,
    enable_window_scaling: bool,

    #[inspect(with = "|x| x.len()")]
    tx_buffer: ring::Ring,
    /// Autotune ceiling for the tx_buffer ring capacity.
    #[inspect(hex)]
    tx_buffer_max: usize,
    #[inspect(with = "inspect_seq")]
    tx_acked: TcpSeqNumber,
    #[inspect(with = "inspect_seq")]
    tx_send: TcpSeqNumber,
    tx_fin_buffered: bool,
    #[inspect(hex)]
    tx_window_len: u16,
    tx_window_scale: u8,
    /// Whether the tx_window_scale is active (i.e., we've received the first
    /// non-SYN ACK). Per RFC 1323 §2.2, the window field in SYN/SYN-ACK
    /// segments is NOT scaled — only subsequent segments are.
    tx_window_scale_active: bool,
    #[inspect(with = "inspect_seq")]
    tx_window_rx_seq: TcpSeqNumber,
    #[inspect(with = "inspect_seq")]
    tx_window_tx_seq: TcpSeqNumber,
    #[inspect(hex)]
    tx_mss: usize,
    #[inspect(skip)]
    last_close_reason: ConnectionCloseReason,

    stats: TcpConnStats,
}

/// Why a connection was closed, for aggregate stats categorization.
#[derive(Default, Clone, Copy)]
enum ConnectionCloseReason {
    #[default]
    LocalError,
    PeerRst,
    Normal,
}

/// Policy for whether `send_data` should emit a standalone (pure) ACK when
/// there is nothing else to put in the segment.
///
/// Pure ACKs are deferred from the per-packet `handle_tcp` hot path so that
/// bursts of inbound guest packets coalesce into a single ACK emitted by the
/// trailing `poll_tcp` cycle. Without this, every inbound data segment
/// triggers a zero-payload ACK back, doubling the packet rate and adding
/// per-packet overhead on the virtual link (RFC 1122 §4.2.3.2 explicitly
/// permits delaying ACKs to coalesce them).
#[derive(Copy, Clone, PartialEq, Eq)]
enum AckPolicy {
    /// Don't emit a standalone ACK in this call. Data segments and FINs
    /// still go out; `needs_ack` remains set so a later `Flush` call (or
    /// a piggybacked ACK on outbound data) will satisfy it.
    Defer,
    /// Emit a standalone ACK if one is pending. Used from poll-cycle paths
    /// that run once per batch (`poll_socket_backend`, `poll_dns_backend`).
    Flush,
}

/// Per-connection TCP statistics for performance analysis.
#[derive(Inspect, Default)]
struct TcpConnStats {
    /// Bytes sent from host to guest.
    bytes_tx_to_guest: Counter,
    /// Payload bytes received from guest to host (excludes pure ACKs and
    /// FIN-only segments).
    bytes_rx_from_guest: Counter,
    /// Data segments sent from host to guest via `send_data` (every such
    /// segment carries an ACK; this does not include standalone ACKs).
    pkts_tx_to_guest: Counter,
    /// Data segments received from guest to host (payload-bearing only;
    /// excludes pure ACKs and FIN-only segments).
    data_segments_rx_from_guest: Counter,
    /// Standalone ACKs sent via `ack()` in response to unacceptable
    /// segments (duplicate, out-of-order, out-of-window). Data segments
    /// sent via `send_data` are counted in `pkts_tx_to_guest` instead.
    standalone_acks_tx: Counter,
    /// RSTs sent.
    rsts_tx: Counter,
    /// Times send_data broke out because rx_mtu was 0 (no guest rx buffers).
    tx_blocked_no_rx_mtu: Counter,
    /// Times send_data was limited by the peer's advertised window being full.
    tx_blocked_window_full: Counter,
    /// Out-of-window packets received.
    out_of_window_pkts: Counter,
    /// Segment size distribution for packets sent to guest.
    tx_segment_size: Histogram<14>,
    /// Segment size distribution for packets received from guest.
    rx_segment_size: Histogram<14>,
    /// Number of times the tx_buffer ring capacity was grown by autotune.
    tx_buffer_grows: Counter,
    /// Number of times the rx_buffer ring capacity was grown by autotune.
    rx_buffer_grows: Counter,
}

fn inspect_seq(seq: &TcpSeqNumber) -> inspect::AsHex<u32> {
    inspect::AsHex(seq.0 as u32)
}

#[derive(Inspect)]
struct TcpListener {
    #[inspect(skip)]
    socket: PolledSocket<Socket>,
    host_port: u16,
}

#[derive(Debug, PartialEq, Eq, Inspect)]
enum TcpState {
    Connecting,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
}

impl TcpState {
    fn tx_fin(&self) -> bool {
        match self {
            TcpState::Connecting
            | TcpState::SynSent
            | TcpState::SynReceived
            | TcpState::Established
            | TcpState::CloseWait => false,

            TcpState::FinWait1
            | TcpState::FinWait2
            | TcpState::Closing
            | TcpState::TimeWait
            | TcpState::LastAck => true,
        }
    }

    fn rx_fin(&self) -> bool {
        match self {
            TcpState::Connecting
            | TcpState::SynSent
            | TcpState::SynReceived
            | TcpState::Established
            | TcpState::FinWait1
            | TcpState::FinWait2 => false,

            TcpState::CloseWait | TcpState::Closing | TcpState::LastAck | TcpState::TimeWait => {
                true
            }
        }
    }
}

impl<T: Client> Access<'_, T> {
    pub(crate) fn poll_tcp(&mut self, cx: &mut Context<'_>) {
        // Check for any new incoming connections
        self.inner
            .tcp
            .listeners
            .retain(|key, listener| match listener.poll_listener(cx) {
                Ok(result) => {
                    if let Some((socket, mut other_addr)) = result {
                        // If this packet was originally from the guest, update the port to match
                        // the original guest port. This allows loopback to work as expected.
                        if self.inner.state.params.is_local_address(&other_addr) {
                            for (other_ft, connection) in self.inner.tcp.connections.iter() {
                                if matches!(connection.inner.state, TcpState::Connecting | TcpState::SynReceived)
                                    && PortForwardKey::from_socket_addr(other_ft.dst, other_ft.dst.port()) == *key
                                {
                                    if let LoopbackPortInfo::ProxyForGuestPort {
                                        sending_port,
                                        guest_port,
                                    } = connection.inner.loopback_port
                                    {
                                        if sending_port == other_addr.port() {
                                            other_addr.set_port(guest_port);
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        let Some(ft) = self.inner.state.try_ft_from_remote_address(&other_addr, key.guest_port) else {
                            return true;
                        };

                        // TCP connections are stored with the source always as the guest. Switch the order.
                        let ft = FourTuple {
                            src: ft.dst,
                            dst: ft.src,
                        };

                        match self.inner.tcp.connections.entry(ft) {
                            hash_map::Entry::Vacant(e) => {
                                let mut sender = Sender {
                                    ft: &ft,
                                    client: self.client,
                                    state: &mut self.inner.state,
                                };

                                let conn = match TcpConnection::new_from_accept(
                                    &mut sender,
                                    socket,
                                    &self.inner.tcp.connection_params,
                                ) {
                                    Ok(conn) => conn,
                                    Err(err) => {
                                        tracing::warn!(
                                            error = &err as &dyn std::error::Error,
                                            src = %ft.src,
                                            dst = %ft.dst,
                                            "Failed to create connection from newly accepted socket",
                                        );
                                        return true;
                                    }
                                };
                                tracing::trace!(?ft, "TCP connection established");
                                e.insert(conn);
                                self.inner.tcp.aggregate_stats.connections_accepted.increment();
                            }
                            hash_map::Entry::Occupied(_) => {
                                tracing::warn!(
                                    src = %ft.src,
                                    dst = %ft.dst,
                                    "New client request ignored because it was already connected"
                                );
                            }
                        }
                    }
                    true
                }
                Err(_) => false,
            });
        // Check for any new incoming data
        self.inner.tcp.connections.retain(|ft, conn| {
            let mut sender = Sender {
                ft,
                state: &mut self.inner.state,
                client: self.client,
            };
            let keep = match &mut conn.backend {
                TcpBackend::Dns(dns_handler) => match &mut self.inner.dns {
                    Some(dns) => conn
                        .inner
                        .poll_dns_backend(cx, &mut sender, dns_handler, dns),
                    None => {
                        tracing::warn!("DNS TCP connection without DNS resolver, dropping");
                        false
                    }
                },
                TcpBackend::Socket(opt_socket) => {
                    conn.inner.poll_socket_backend(cx, &mut sender, opt_socket)
                }
            };
            if !keep {
                self.inner
                    .tcp
                    .aggregate_stats
                    .record_close(conn.inner.last_close_reason);
            }
            keep
        })
    }

    pub(crate) fn refresh_tcp_driver(&mut self) {
        self.inner.tcp.connections.retain(|ft, conn| {
            let TcpBackend::Socket(opt_socket) = &mut conn.backend else {
                // DNS connections have no real socket to refresh.
                return true;
            };
            let Some(socket) = opt_socket.take() else {
                return true;
            };
            let socket = socket.into_inner();
            match PolledSocket::new(self.client.driver(), socket) {
                Ok(socket) => {
                    *opt_socket = Some(socket);
                    true
                }
                Err(err) => {
                    tracing::warn!(
                        error = &err as &dyn std::error::Error,
                        src = %ft.src,
                        dst = %ft.dst,
                        "failed to update driver for tcp connection"
                    );
                    false
                }
            }
        });
    }

    pub(crate) fn handle_tcp(
        &mut self,
        addresses: &IpAddresses,
        payload: &[u8],
        checksum: &ChecksumState,
    ) -> Result<(), DropReason> {
        let tcp_packet = TcpPacket::new_checked(payload)?;
        let tcp = TcpRepr::parse(
            &tcp_packet,
            &addresses.src_addr(),
            &addresses.dst_addr(),
            &checksum.caps(),
        )?;

        let ft = match addresses {
            IpAddresses::V4(addresses) => FourTuple {
                dst: SocketAddr::V4(SocketAddrV4::new(addresses.dst_addr, tcp.dst_port)),
                src: SocketAddr::V4(SocketAddrV4::new(addresses.src_addr, tcp.src_port)),
            },
            IpAddresses::V6(addresses) => FourTuple {
                dst: SocketAddr::V6(SocketAddrV6::new(addresses.dst_addr, tcp.dst_port, 0, 0)),
                src: SocketAddr::V6(SocketAddrV6::new(addresses.src_addr, tcp.src_port, 0, 0)),
            },
        };
        trace_tcp_packet(&ft, &tcp, tcp.payload.len(), "recv");

        let is_dns_tcp =
            is_gateway_dns_tcp(&ft, &self.inner.state.params, self.inner.dns.is_some());

        let mut sender = Sender {
            ft: &ft,
            client: self.client,
            state: &mut self.inner.state,
        };

        match self.inner.tcp.connections.entry(ft) {
            hash_map::Entry::Occupied(mut e) => {
                let keep = e.get_mut().inner.handle_packet(&mut sender, &tcp)?;
                if keep {
                    // Push out any newly-unblocked data (e.g., this ACK advanced
                    // the peer window) so we don't wait an entire poll cycle.
                    //
                    // Use `AckPolicy::Defer` so we DON'T emit a standalone ACK
                    // here: inbound bursts arrive as many back-to-back
                    // `handle_tcp` calls within a single `poll_ready` cycle,
                    // and the trailing `poll_tcp` will emit (at most) one
                    // consolidated ACK for the whole batch — or piggyback it
                    // on outbound data if any becomes available. Without this,
                    // every guest packet would trigger a zero-payload ACK back,
                    // doubling packet rate and creating an ACK storm.
                    e.get_mut().inner.send_next(&mut sender, AckPolicy::Defer);
                } else {
                    self.inner
                        .tcp
                        .aggregate_stats
                        .record_close(e.get().inner.last_close_reason);
                    let dns_in_flight = matches!(
                        e.get().backend,
                        TcpBackend::Dns(ref h) if h.is_in_flight()
                    );
                    e.remove();
                    if dns_in_flight {
                        if let Some(dns) = &mut self.inner.dns {
                            dns.complete_tcp_query();
                        }
                    }
                }
            }
            hash_map::Entry::Vacant(e) => {
                if tcp.control == TcpControl::Rst {
                    // This connection is already closed. Ignore the packet.
                } else if let Some(ack) = tcp.ack_number {
                    // This is for an old connection. Send reset.
                    sender.rst(ack, None);
                } else if tcp.control == TcpControl::Syn {
                    let conn = if is_dns_tcp {
                        TcpConnection::new_dns(
                            &mut sender,
                            &tcp,
                            &self.inner.tcp.connection_params,
                        )?
                    } else {
                        // Resolve virtual mapped addresses back to real host
                        // addresses before establishing the connection.
                        let resolved_dst = sender.state.resolve_destination(&sender.ft.dst);
                        // If this is directed to a local port owned by the guest, use the
                        // appropriate host port substitution.
                        let is_local_address = sender.state.params.is_local_address(&resolved_dst);
                        let key =
                            PortForwardKey::from_socket_addr(resolved_dst, resolved_dst.port());
                        let ft = if is_local_address
                            && let Some(listener) = self.inner.tcp.listeners.get(&key)
                        {
                            FourTuple {
                                src: sender.ft.src,
                                dst: SocketAddr::new(resolved_dst.ip(), listener.host_port),
                            }
                        } else if resolved_dst != sender.ft.dst {
                            FourTuple {
                                src: sender.ft.src,
                                dst: resolved_dst,
                            }
                        } else {
                            ft
                        };
                        let mut sender = Sender {
                            ft: &ft,
                            client: sender.client,
                            state: sender.state,
                        };
                        TcpConnection::new(
                            &mut sender,
                            &tcp,
                            &self.inner.tcp.connection_params,
                            is_local_address,
                        )?
                    };
                    e.insert(conn);
                    self.inner
                        .tcp
                        .aggregate_stats
                        .connections_initiated
                        .increment();
                } else {
                    // Ignore the packet.
                }
            }
        }
        Ok(())
    }

    /// Binds to the specified host IP and port for listening for incoming
    /// connections.
    pub fn bind_tcp_port(&mut self, socket: Socket, guest_port: u16) -> Result<(), BindError> {
        let host_addr = Self::socket_local_addr(&socket)?;
        let key = PortForwardKey::from_socket_addr(host_addr, guest_port);
        match self.inner.tcp.listeners.entry(key) {
            hash_map::Entry::Occupied(_) => {
                return Err(BindError::PortAlreadyBound(guest_port));
            }
            hash_map::Entry::Vacant(e) => {
                let listener = TcpListener::from_socket(self.client.driver(), socket)?;
                e.insert(listener);
            }
        };
        Ok(())
    }

    /// Unbinds from the specified guest port and IP family.
    pub fn unbind_tcp_port(&mut self, family: IpVersion, port: u16) -> Result<(), BindError> {
        match self
            .inner
            .tcp
            .listeners
            .entry(PortForwardKey::new(family, port))
        {
            hash_map::Entry::Occupied(e) => {
                e.remove();
                Ok(())
            }
            hash_map::Entry::Vacant(_) => Err(BindError::PortNotBound),
        }
    }

    fn socket_local_addr(socket: &Socket) -> Result<SocketAddr, BindError> {
        socket
            .local_addr()
            .map_err(BindError::Io)?
            .as_socket()
            .ok_or_else(|| BindError::Io(io::Error::other("socket local address is invalid")))
    }
}

struct Sender<'a, T> {
    ft: &'a FourTuple,
    client: &'a mut T,
    state: &'a mut ConsommeState,
}

impl<T: Client> Sender<'_, T> {
    fn send_packet(&mut self, tcp: &TcpRepr<'_>, payload: Option<ring::View<'_>>) {
        let payload_len = payload.as_ref().map_or(0, |p| p.len());
        let buffer = &mut self.state.buffer;
        let mut eth_packet = EthernetFrame::new_unchecked(&mut buffer[..]);
        eth_packet.set_dst_addr(self.state.params.client_mac);
        eth_packet.set_src_addr(self.state.params.gateway_mac);
        let ip = IpRepr::new(
            self.ft.dst.ip().into(),
            self.ft.src.ip().into(),
            IpProtocol::Tcp,
            tcp.header_len() + payload_len,
            64,
        );
        // Set the ethernet type based on IP version
        match ip {
            IpRepr::Ipv4(_) => eth_packet.set_ethertype(EthernetProtocol::Ipv4),
            IpRepr::Ipv6(_) => eth_packet.set_ethertype(EthernetProtocol::Ipv6),
        }

        // Emit IP packet and get the TCP payload buffer (works for both IPv4 and IPv6)
        let ip_packet_buf = eth_packet.payload_mut();
        ip.emit(&mut *ip_packet_buf, &ChecksumCapabilities::default());

        let (tcp_payload_buf, ip_total_len) = match self.ft.dst {
            SocketAddr::V4(_) => {
                let ipv4_packet = Ipv4Packet::new_unchecked(&*ip_packet_buf);
                let total_len = ipv4_packet.total_len() as usize;
                let payload_offset = ipv4_packet.header_len() as usize;
                (&mut ip_packet_buf[payload_offset..total_len], total_len)
            }
            SocketAddr::V6(_) => {
                let ipv6_packet = Ipv6Packet::new_unchecked(&*ip_packet_buf);
                let total_len = ipv6_packet.total_len();
                let payload_offset = IPV6_HEADER_LEN;
                (&mut ip_packet_buf[payload_offset..total_len], total_len)
            }
        };

        let dst_ip_addr: IpAddress = self.ft.dst.ip().into();
        let src_ip_addr: IpAddress = self.ft.src.ip().into();
        let mut tcp_packet = TcpPacket::new_unchecked(tcp_payload_buf);
        // Checksums are computed explicitly below, so skip smoltcp's pass.
        tcp.emit(
            &mut tcp_packet,
            &dst_ip_addr,
            &src_ip_addr,
            &ChecksumCapabilities::ignored(),
        );

        let checksum_state = match self.ft.dst {
            SocketAddr::V4(_) => ChecksumState::TCP4,
            SocketAddr::V6(_) => ChecksumState::TCP6,
        };
        let n = ETHERNET_HEADER_LEN + ip_total_len;

        let payload = match payload {
            Some(p) if p.len() != 0 => p,
            _ => {
                tcp_packet.fill_checksum(&dst_ip_addr, &src_ip_addr);
                let buffer = &self.state.buffer;
                self.client.recv(&buffer[..n], &checksum_state);
                return;
            }
        };

        // Zero-copy payload path: leave the TCP window bytes in place and hand
        // the header and (up to two) payload slices to the client as
        // discontiguous segments, avoiding a copy of the payload into the
        // header buffer. The TCP checksum must be recomputed across those
        // segments because the payload was never linearized here.
        let (a, b) = payload.as_slices();
        let tcp_header_len = tcp_packet.header_len() as usize;
        tcp_packet.set_checksum(0);
        // The TCP header length is a multiple of 4, so the header and `a` are
        // 16-bit aligned; `b` shifts by a byte only when `a` has odd length, in
        // which case its partial checksum is byte-swapped.
        let checksum = !checksum::combine(&[
            checksum::pseudo_header(
                &src_ip_addr,
                &dst_ip_addr,
                IpProtocol::Tcp,
                (tcp_header_len + payload_len) as u32,
            ),
            checksum::data(&tcp_packet.as_ref()[..tcp_header_len]),
            checksum::data(a),
            if a.len() % 2 == 0 {
                checksum::data(b)
            } else {
                checksum::data(b).swap_bytes()
            },
        ]);
        tcp_packet.set_checksum(checksum);

        let header_len = n - payload_len;
        let buffer = &self.state.buffer;
        let header = &buffer[..header_len];
        if b.is_empty() {
            self.client.recv_segments(&[header, a], &checksum_state);
        } else {
            self.client.recv_segments(&[header, a, b], &checksum_state);
        }
    }

    fn rst(&mut self, seq: TcpSeqNumber, ack: Option<TcpSeqNumber>) {
        let tcp = TcpRepr {
            src_port: self.ft.dst.port(),
            dst_port: self.ft.src.port(),
            control: TcpControl::Rst,
            seq_number: seq,
            ack_number: ack,
            window_len: 0,
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None, None, None],
            timestamp: None,
            payload: &[],
        };

        trace_tcp_packet(self.ft, &tcp, 0, "rst xmit");

        self.send_packet(&tcp, None);
    }
}

impl TcpConnection {
    fn new_base(params: &ConnectionParams) -> TcpConnectionInner {
        let mut rx_tx_seq = [0; 8];
        getrandom::fill(&mut rx_tx_seq[..]).expect("prng failure");
        let rx_seq = TcpSeqNumber(i32::from_ne_bytes(
            rx_tx_seq[0..4].try_into().expect("invalid length"),
        ));
        let tx_seq = TcpSeqNumber(i32::from_ne_bytes(
            rx_tx_seq[4..8].try_into().expect("invalid length"),
        ));

        let rx_bounds = params.rx_buffer;
        let rx_window_scale =
            (usize::BITS - rx_bounds.max.leading_zeros()).saturating_sub(16) as u8;

        let tx_bounds = params.tx_buffer;

        TcpConnectionInner {
            loopback_port: LoopbackPortInfo::None,
            state: TcpState::Connecting,
            rx_buffer: ring::Ring::new(0),
            rx_window_cap: rx_bounds.initial,
            rx_window_scale,
            rx_buffer_max: rx_bounds.max,
            rx_seq,
            rx_window_last_adv: rx_bounds.initial,
            rx_assembler: assembler::Assembler::new(),
            needs_ack: false,
            is_shutdown: false,
            enable_window_scaling: false,
            tx_buffer: ring::Ring::new(tx_bounds.initial),
            tx_buffer_max: tx_bounds.max,
            tx_acked: tx_seq,
            tx_send: tx_seq,
            tx_window_len: 1,
            tx_window_scale: 0,
            tx_window_scale_active: false,
            tx_window_rx_seq: rx_seq,
            tx_window_tx_seq: tx_seq,
            // The TCPv4 default maximum segment size is 536. This can be bigger for
            // IPv6.
            tx_mss: 536,
            tx_fin_buffered: false,
            last_close_reason: ConnectionCloseReason::LocalError,
            stats: TcpConnStats::default(),
        }
    }

    fn new(
        sender: &mut Sender<'_, impl Client>,
        tcp: &TcpRepr<'_>,
        params: &ConnectionParams,
        is_local_address: bool,
    ) -> Result<Self, DropReason> {
        let mut inner = Self::new_base(params);
        inner.initialize_from_first_client_packet(tcp)?;

        let socket = Socket::new(
            match sender.ft.dst {
                SocketAddr::V4(_) => Domain::IPV4,
                SocketAddr::V6(_) => Domain::IPV6,
            },
            Type::STREAM,
            Some(Protocol::TCP),
        )
        .map_err(DropReason::Io)?;

        // Disable Nagle's algorithm to reduce latency for small packets.
        socket.set_tcp_nodelay(true).map_err(DropReason::Io)?;

        // On Windows the default behavior for non-existent loopback sockets is
        // to wait and try again. This is different than the Linux behavior of
        // immediately failing. Default to the Linux behavior.
        #[cfg(windows)]
        if sender.ft.dst.ip().is_loopback() {
            if let Err(err) = crate::windows::disable_connection_retries(&socket) {
                tracing::trace!(err, "Failed to disable loopback retries");
            }
        }

        let socket = PolledSocket::new(sender.client.driver(), socket).map_err(DropReason::Io)?;
        match socket.get().connect(&SockAddr::from(sender.ft.dst)) {
            Ok(_) => unreachable!(),
            Err(err) if is_connect_incomplete_error(&err) => (),
            Err(err) => {
                log_connect_error(sender.ft, &err);
                sender.rst(TcpSeqNumber(0), Some(tcp.seq_number + tcp.segment_len()));
                return Err(DropReason::Io(err));
            }
        }
        if is_local_address && let Ok(addr) = socket.get().local_addr() {
            match addr.as_socket() {
                None => {
                    tracing::warn!(
                        src = %sender.ft.src,
                        dst = %sender.ft.dst,
                        "unable to get local socket address",
                    );
                }
                Some(addr) => {
                    inner.loopback_port = LoopbackPortInfo::ProxyForGuestPort {
                        sending_port: addr.port(),
                        guest_port: sender.ft.src.port(),
                    };
                }
            }
        }
        Ok(Self {
            backend: TcpBackend::Socket(Some(socket)),
            inner,
        })
    }

    fn new_from_accept(
        sender: &mut Sender<'_, impl Client>,
        socket: Socket,
        params: &ConnectionParams,
    ) -> Result<Self, DropReason> {
        // Disable Nagle's algorithm to reduce latency for small packets.
        socket.set_tcp_nodelay(true).map_err(DropReason::Io)?;

        let mut inner = TcpConnectionInner {
            state: TcpState::SynSent,
            enable_window_scaling: true,
            ..Self::new_base(params)
        };
        inner.send_syn(sender, None);
        Ok(Self {
            backend: TcpBackend::Socket(Some(
                PolledSocket::new(sender.client.driver(), socket).map_err(DropReason::Io)?,
            )),
            inner,
        })
    }

    /// Create a virtual DNS TCP connection (no real host socket).
    /// The connection completes the TCP handshake with the guest and
    /// routes DNS queries through the provided resolver backend.
    fn new_dns(
        sender: &mut Sender<'_, impl Client>,
        tcp: &TcpRepr<'_>,
        params: &ConnectionParams,
    ) -> Result<Self, DropReason> {
        let mut inner = Self::new_base(params);
        inner.initialize_from_first_client_packet(tcp)?;

        let flow = crate::dns_resolver::DnsFlow {
            src: sender.ft.src,
            dst: sender.ft.dst,
            gateway_mac: sender.state.params.gateway_mac,
            client_mac: sender.state.params.client_mac,
            transport: crate::dns_resolver::DnsTransport::Tcp,
        };

        // Immediately transition to SynReceived so the handshake SYN-ACK is sent.
        inner.state = TcpState::SynReceived;
        inner.send_syn(sender, Some(inner.rx_seq));

        Ok(Self {
            backend: TcpBackend::Dns(DnsTcpHandler::new(flow)),
            inner,
        })
    }
}

impl TcpConnectionInner {
    fn initialize_from_first_client_packet(&mut self, tcp: &TcpRepr<'_>) -> Result<(), DropReason> {
        // The TCPv4 default maximum segment size is 536. This can be bigger for
        // IPv6.
        let tx_mss = tcp.max_seg_size.map_or(536, |x| x.into());

        if let Some(tx_window_scale) = tcp.window_scale {
            if tx_window_scale > 14 {
                return Err(TcpError::InvalidWindowScale.into());
            }
            self.enable_window_scaling = true;
            self.tx_window_scale = tx_window_scale;
        } else {
            // Disable rx window scale. Cap the buffer and window to u16::MAX
            // since without window scaling, the window field is only 16 bits.
            self.enable_window_scaling = false;
            self.rx_window_cap = self.rx_window_cap.min(u16::MAX as usize);
            self.rx_buffer_max = self.rx_buffer_max.min(u16::MAX as usize);
            self.rx_window_scale = 0;
        }

        self.rx_buffer = ring::Ring::new(self.rx_window_cap.next_power_of_two());
        self.rx_seq = tcp.seq_number + 1;
        self.tx_window_rx_seq = tcp.seq_number + 1;
        self.tx_mss = tx_mss;
        Ok(())
    }

    /// Poll the DNS TCP virtual connection backend.
    ///
    /// There is no real socket; data flows through the [`DnsTcpHandler`].
    fn poll_dns_backend(
        &mut self,
        cx: &mut Context<'_>,
        sender: &mut Sender<'_, impl Client>,
        dns_handler: &mut DnsTcpHandler,
        dns: &mut DnsResolver,
    ) -> bool {
        // Propagate guest FIN before the tx path so that poll_read can
        // detect EOF on the same iteration.
        if self.state.rx_fin() && !dns_handler.guest_fin() {
            tracing::trace!(
                src = %sender.ft.src,
                dst = %sender.ft.dst,
                tx_buffer_len = self.tx_buffer.len(),
                tx_buffer_full = self.tx_buffer.is_full(),
                "tcp: guest FIN received, signaling EOF to DNS handler",
            );
            dns_handler.set_guest_fin();
        }

        // tx path first: drain DNS responses into tx_buffer.
        // This frees up backpressure so that ingest can make progress.
        while !self.tx_buffer.is_full() {
            let (a, b) = self.tx_buffer.unwritten_slices_mut();
            let mut bufs = [IoSliceMut::new(a), IoSliceMut::new(b)];
            match dns_handler.poll_read(cx, &mut bufs, dns) {
                Poll::Ready(Ok(n)) => {
                    if n == 0 {
                        // EOF — close the connection.
                        if !self.state.tx_fin() {
                            self.close();
                        }
                        break;
                    }
                    self.tx_buffer.extend_by(n);
                    tracing::trace!(
                        src = %sender.ft.src,
                        dst = %sender.ft.dst,
                        n,
                        tx_buffer_len = self.tx_buffer.len(),
                        tx_buffer_full = self.tx_buffer.is_full(),
                        "tcp: response from DNS handler into tx_buffer",
                    );
                }
                Poll::Ready(Err(_)) => {
                    sender.rst(self.tx_send, Some(self.rx_seq));
                    self.stats.rsts_tx.increment();
                    return false;
                }
                Poll::Pending => break,
            }
        }

        // rx path: feed guest data into the DNS handler for query extraction.
        let view = self.rx_buffer.view(0..self.rx_buffer.len());
        let (a, b) = view.as_slices();
        match dns_handler.ingest(&[a, b], dns) {
            Ok(consumed) if consumed > 0 => {
                self.rx_buffer.consume(consumed);
            }
            Ok(_) => {}
            Err(_) => {
                // Invalid DNS TCP framing; reset the connection.
                sender.rst(self.tx_send, Some(self.rx_seq));
                self.stats.rsts_tx.increment();
                return false;
            }
        }

        // Flush any deferred pure-ACK from per-packet `handle_tcp` calls.
        self.send_next(sender, AckPolicy::Flush);
        let closing = self.state == TcpState::TimeWait
            || self.state == TcpState::LastAck
            || (self.state.tx_fin() && self.state.rx_fin() && self.tx_buffer.is_empty());
        if closing {
            self.last_close_reason = ConnectionCloseReason::Normal;
        }
        !closing
    }

    /// Poll the real-socket TCP connection backend.
    ///
    /// Reads data from the host socket into the tx buffer (host -> guest) and
    /// writes guest rx data into the host socket (guest -> host).
    fn poll_socket_backend(
        &mut self,
        cx: &mut Context<'_>,
        sender: &mut Sender<'_, impl Client>,
        opt_socket: &mut Option<PolledSocket<Socket>>,
    ) -> bool {
        // Wait for the outbound connection to complete.
        if self.state == TcpState::Connecting {
            let Some(socket) = opt_socket.as_mut() else {
                return false;
            };
            match socket.poll_ready(cx, PollEvents::OUT) {
                Poll::Ready(r) => {
                    if r.has_err() {
                        self.handle_connect_error(sender, socket);
                        return false;
                    }

                    tracing::debug!(
                        src = %sender.ft.src,
                        dst = %sender.ft.dst,
                        "connection established",
                    );
                    self.state = TcpState::SynReceived;
                }
                Poll::Pending => return true,
            }
        } else if self.state == TcpState::SynSent {
            // Need to establish connection with client before sending data.
            return true;
        }

        // Handle the tx path.
        if let Some(socket) = opt_socket.as_mut() {
            if self.state.tx_fin() {
                if let Poll::Ready(events) = socket.poll_ready(cx, PollEvents::EMPTY) {
                    if events.has_err() {
                        let err = take_socket_error(socket);
                        match err.kind() {
                            ErrorKind::BrokenPipe | ErrorKind::ConnectionReset => {}
                            _ => tracelimit::warn_ratelimited!(
                                error = &err as &dyn std::error::Error,
                                src = %sender.ft.src,
                                dst = %sender.ft.dst,
                                "socket failure after fin"
                            ),
                        }
                        sender.rst(self.tx_send, Some(self.rx_seq));
                        self.stats.rsts_tx.increment();
                        return false;
                    }

                    // Both ends are closed. Close the actual socket.
                    *opt_socket = None;
                }
            } else {
                // Drain the host socket into the tx ring until the socket has
                // no more data (Pending) or the ring reaches its autotune
                // ceiling. When the ring fills with data still pending, grow it
                // (doubling, capped at tx_buffer_max) and keep reading so the
                // freshly added capacity is used in this same poll rather than
                // waiting for a later guest ACK to re-clock the connection.
                'read: loop {
                    while !self.tx_buffer.is_full() {
                        let (a, b) = self.tx_buffer.unwritten_slices_mut();
                        let mut bufs = [IoSliceMut::new(a), IoSliceMut::new(b)];
                        match Pin::new(&mut *socket).poll_read_vectored(cx, &mut bufs) {
                            Poll::Ready(Ok(n)) => {
                                if n == 0 {
                                    self.close();
                                    break 'read;
                                }
                                self.tx_buffer.extend_by(n);
                            }
                            Poll::Ready(Err(err)) => {
                                match err.kind() {
                                    ErrorKind::ConnectionReset => tracing::trace!(
                                        error = &err as &dyn std::error::Error,
                                        src = %sender.ft.src,
                                        dst = %sender.ft.dst,
                                        "socket read error"
                                    ),
                                    _ => tracelimit::warn_ratelimited!(
                                        error = &err as &dyn std::error::Error,
                                        src = %sender.ft.src,
                                        dst = %sender.ft.dst,
                                        "socket read error"
                                    ),
                                }
                                sender.rst(self.tx_send, Some(self.rx_seq));
                                self.stats.rsts_tx.increment();
                                return false;
                            }
                            Poll::Pending => break 'read,
                        }
                    }

                    // The ring is full. If we can still grow, double it (capped
                    // at tx_buffer_max) and keep reading into the new space. At
                    // the ceiling we stop without re-arming: the cached socket
                    // readiness stays set and the next guest ACK that drains the
                    // ring re-clocks the read, which is safe on edge-triggered
                    // epoll backends.
                    if self.tx_buffer.capacity() >= self.tx_buffer_max {
                        break;
                    }
                    let new_cap = (self.tx_buffer.capacity() * 2).min(self.tx_buffer_max);
                    self.tx_buffer.resize(new_cap);
                    self.stats.tx_buffer_grows.increment();
                }
            }
        }

        // Handle the rx path.
        if let Some(socket) = opt_socket.as_mut() {
            let rx_high_water = self.rx_buffer.len();
            while !self.rx_buffer.is_empty() {
                let view = self.rx_buffer.view(0..self.rx_buffer.len());
                let (a, b) = view.as_slices();
                let bufs = [IoSlice::new(a), IoSlice::new(b)];
                match Pin::new(&mut *socket).poll_write_vectored(cx, &bufs) {
                    Poll::Ready(Ok(n)) => {
                        self.rx_buffer.consume(n);
                    }
                    Poll::Ready(Err(err)) => {
                        match err.kind() {
                            ErrorKind::BrokenPipe | ErrorKind::ConnectionReset => {}
                            _ => {
                                tracelimit::warn_ratelimited!(
                                    error = &err as &dyn std::error::Error,
                                    src = %sender.ft.src,
                                    dst = %sender.ft.dst,
                                    "socket write error"
                                );
                            }
                        }
                        sender.rst(self.tx_send, Some(self.rx_seq));
                        self.stats.rsts_tx.increment();
                        return false;
                    }
                    Poll::Pending => break,
                }
            }
            // Draining to the host may have reopened the window; re-advertise it
            // so the guest resumes without waiting on its persist timer.
            if self.should_reopen_window() {
                self.needs_ack = true;
            }
            // Autotune: if the host kept up (drained to empty) and the buffer
            // was at least 75% full this cycle, the guest is rx-bound. Grow
            // both the ring and the advertised window ceiling so the next ACK
            // tells the guest it can send more. Gated on the assembler being
            // empty because `Ring::resize` only preserves contiguous bytes
            // in `[head, tail)` — any out-of-order data staged past `tail`
            // via `write_at` would be lost.
            if self.rx_buffer.is_empty()
                && self.rx_assembler.is_empty()
                && self.rx_window_cap < self.rx_buffer_max
                && rx_high_water * 4 >= self.rx_window_cap * 3
            {
                let new_cap = (self.rx_window_cap * 2).min(self.rx_buffer_max);
                let new_ring_cap = new_cap.next_power_of_two();
                if new_ring_cap > self.rx_buffer.capacity() {
                    self.rx_buffer.resize(new_ring_cap);
                }
                self.rx_window_cap = new_cap;
                self.needs_ack = true;
                self.stats.rx_buffer_grows.increment();
            }
            if self.rx_buffer.is_empty() && self.state.rx_fin() && !self.is_shutdown {
                if let Err(err) = socket.get().shutdown(Shutdown::Write) {
                    tracelimit::warn_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        src = %sender.ft.src,
                        dst = %sender.ft.dst,
                        "shutdown error"
                    );
                    sender.rst(self.tx_send, Some(self.rx_seq));
                    self.stats.rsts_tx.increment();
                    return false;
                }
                self.is_shutdown = true;
            }
        }

        // Send any pending data or ACKs. Always use Flush: if no data was
        // read from the socket and no ACK is pending, send_data will find
        // nothing to do anyway.
        self.send_next(sender, AckPolicy::Flush);
        true
    }

    fn handle_connect_error(
        &mut self,
        sender: &mut Sender<'_, impl Client>,
        socket: &mut PolledSocket<Socket>,
    ) {
        let err = take_socket_error(socket);
        if err.kind() == ErrorKind::TimedOut {
            // Avoid resetting so that the guest doesn't think there is a
            // responding TCP stack at this address. The guest will time out on
            // its own.
            tracing::debug!(
                src = %sender.ft.src,
                dst = %sender.ft.dst,
                error = &err as &dyn std::error::Error,
                "connect timed out",
            );
        } else {
            log_connect_error(sender.ft, &err);
            sender.rst(self.tx_send, Some(self.rx_seq));
            self.stats.rsts_tx.increment();
        }
    }

    fn rx_window_len(&self) -> u16 {
        (self.rx_window_avail() >> self.rx_window_scale) as u16
    }

    /// Available receive window in unscaled bytes.
    fn rx_window_avail(&self) -> usize {
        self.rx_window_cap - self.rx_buffer.len()
    }

    /// The receive window as the guest reconstructs it: `rx_window_avail`
    /// rounded down to the scale quantum, since the low `rx_window_scale` bits
    /// are not transmitted. Equals `rx_window_avail` when scaling is off.
    fn rx_window_advertised(&self) -> usize {
        (self.rx_window_avail() >> self.rx_window_scale) << self.rx_window_scale
    }

    /// Record the window just advertised to the guest. `window_len` is the
    /// scaled value placed on the wire; the guest shifts it back up by the
    /// window scale, so track that reconstructed value to keep the reopen
    /// check accurate.
    fn record_advertised_window(&mut self, window_len: u16) {
        self.rx_window_last_adv = (window_len as usize) << self.rx_window_scale;
    }

    /// Whether draining to the host reopened the receive window enough to
    /// re-advertise it (RFC 1122 §4.2.3.3 receiver SWS avoidance). Fires once on
    /// the closed-to-open transition, once the guest-visible (post-scale-
    /// truncation) window reaches the reopen threshold: a full segment, or half
    /// the window cap when the cap is smaller than one segment.
    fn should_reopen_window(&self) -> bool {
        let reopen_threshold = self.tx_mss.min(self.rx_window_cap / 2);
        self.rx_window_last_adv < reopen_threshold
            && self.rx_window_advertised() >= reopen_threshold
    }

    fn send_next(&mut self, sender: &mut Sender<'_, impl Client>, ack_policy: AckPolicy) {
        match self.state {
            TcpState::Connecting => {}
            TcpState::SynReceived => self.send_syn(sender, Some(self.rx_seq)),
            _ => self.send_data(sender, ack_policy),
        }
    }

    fn send_syn(&mut self, sender: &mut Sender<'_, impl Client>, ack_number: Option<TcpSeqNumber>) {
        if self.tx_send != self.tx_acked || sender.client.rx_mtu() == 0 {
            return;
        }

        // If the client side specified a window scale option, then do the same
        // (even with no shift) to enable window scale support.
        let window_scale = self.enable_window_scaling.then_some(self.rx_window_scale);

        // RFC 7323 §2.2: the window in a SYN/SYN-ACK is not scaled even with the
        // window_scale option present. Advertise the real window clamped to 16
        // bits; the active-open SYN (no ACK) carries a zero window.
        let window_len = if ack_number.is_some() {
            self.rx_window_avail().min(u16::MAX as usize) as u16
        } else {
            0
        };

        // Advertise the maximum possible segment size, allowing the guest
        // to truncate this to its own MTU calculation.
        let max_seg_size = u16::MAX;
        let tcp = TcpRepr {
            src_port: sender.ft.dst.port(),
            dst_port: sender.ft.src.port(),
            control: TcpControl::Syn,
            seq_number: self.tx_send,
            ack_number,
            window_len,
            window_scale,
            max_seg_size: Some(max_seg_size),
            sack_permitted: false,
            sack_ranges: [None, None, None],
            timestamp: None,
            payload: &[],
        };

        sender.send_packet(&tcp, None);
        self.tx_send += 1;
        if ack_number.is_some() {
            // The guest reads the SYN-ACK window unscaled, so record that value.
            self.rx_window_last_adv = window_len as usize;
        }
    }

    fn send_data(&mut self, sender: &mut Sender<'_, impl Client>, ack_policy: AckPolicy) {
        // RFC 1323 §2.2: the window field in SYN/SYN-ACK is unscaled. Only
        // apply the shift once the handshake is complete (first non-SYN window
        // update sets tx_window_scale_active). For the guest-initiated path
        // this is set before send_data can run; for host-initiated (port-forward)
        // connections it guards against using the unscaled SYN-ACK window.
        let scale = if self.tx_window_scale_active {
            self.tx_window_scale
        } else {
            0
        };
        let tx_payload_end = self.tx_acked + self.tx_buffer.len();
        let tx_end = tx_payload_end + self.tx_fin_buffered as usize;
        let tx_window_end = self.tx_acked + ((self.tx_window_len as usize) << scale);
        let tx_done = seq_min([tx_end, tx_window_end]);

        if self.tx_send < tx_end && tx_window_end <= self.tx_send {
            self.stats.tx_blocked_window_full.increment();
        }

        while self.needs_ack || self.tx_send < tx_done {
            let rx_mtu = sender.client.rx_mtu();
            if rx_mtu == 0 {
                // Out of receive buffers.
                self.stats.tx_blocked_no_rx_mtu.increment();
                break;
            }

            let window_len = self.rx_window_len();
            let mut tcp = TcpRepr {
                src_port: sender.ft.dst.port(),
                dst_port: sender.ft.src.port(),
                control: TcpControl::None,
                seq_number: self.tx_send,
                ack_number: Some(self.rx_seq),
                window_len,
                window_scale: None,
                max_seg_size: None,
                sack_permitted: false,
                sack_ranges: [None, None, None],
                timestamp: None,
                payload: &[],
            };

            let mut tx_next = self.tx_send;

            // Compute the end of the segment buffer in sequence space to avoid
            // exceeding:
            // 1. The available buffer length.
            // 2. The current window.
            // 3. The configured maximum segment size.
            // 4. The client MTU.
            let tx_segment_end = {
                let ip_header_len = match sender.ft.dst {
                    SocketAddr::V4(_) => IPV4_HEADER_LEN,
                    SocketAddr::V6(_) => IPV6_HEADER_LEN,
                };
                let header_len = ETHERNET_HEADER_LEN + ip_header_len + tcp.header_len();
                let mtu = rx_mtu.min(sender.state.buffer.len());
                seq_min([
                    tx_payload_end,
                    tx_window_end,
                    tx_next + self.tx_mss,
                    tx_next + (mtu - header_len),
                ])
            };

            let (payload_start, payload_len) = if tx_next < tx_segment_end {
                (tx_next - self.tx_acked, tx_segment_end - tx_next)
            } else {
                (0, 0)
            };

            tx_next += payload_len;

            // Set PSH on the segment that drains all currently-buffered data.
            // This tells the guest TCP stack to deliver the data to the
            // application immediately rather than waiting for more.
            // Note: when tx_fin_buffered is true, the FIN block below will
            // override tcp.control to Fin, which takes priority over Psh.
            if payload_len > 0 && tx_next == tx_payload_end && !self.tx_fin_buffered {
                tcp.control = TcpControl::Psh;
            }

            // Include the fin if present if there is still room.
            if self.tx_fin_buffered
                && tcp.control != TcpControl::Fin
                && tx_next == tx_payload_end
                && tx_next < tx_window_end
            {
                tcp.control = TcpControl::Fin;
                tx_next += 1;
            }

            // If this iteration would emit a pure ACK (no payload, no FIN)
            // and the caller asked us to defer pure ACKs, stop the loop.
            // `needs_ack` is left set for the next poll-cycle `Flush` call
            // (or a piggybacked ACK on later outbound data).
            if ack_policy == AckPolicy::Defer
                && tx_next == self.tx_send
                && tcp.control == TcpControl::None
            {
                break;
            }

            assert!(tx_next <= tx_end);
            assert!(self.needs_ack || tx_next > self.tx_send);

            trace_tcp_packet(sender.ft, &tcp, payload_len, "xmit");

            let payload = self
                .tx_buffer
                .view(payload_start..payload_start + payload_len);

            sender.send_packet(&tcp, Some(payload));
            self.stats.pkts_tx_to_guest.increment();
            self.stats.bytes_tx_to_guest.add(payload_len as u64);
            self.stats.tx_segment_size.add_sample(payload_len as u64);
            self.tx_send = tx_next;
            self.needs_ack = false;
            self.record_advertised_window(window_len);
        }

        assert!(self.tx_send <= tx_end);
    }

    fn close(&mut self) {
        tracing::trace!("fin");
        match self.state {
            TcpState::SynSent | TcpState::SynReceived | TcpState::Established => {
                self.state = TcpState::FinWait1;
            }
            TcpState::CloseWait => {
                self.state = TcpState::LastAck;
            }
            TcpState::Connecting
            | TcpState::FinWait1
            | TcpState::FinWait2
            | TcpState::Closing
            | TcpState::TimeWait
            | TcpState::LastAck => unreachable!("fin in {:?}", self.state),
        }
        self.tx_fin_buffered = true;
    }

    /// Send an ACK using the current state of the connection.
    ///
    /// This is used when sending an ack to report a the reception of an
    /// unacceptable packet (duplicate, out of order, etc.). These acks
    /// shouldn't be combined with data so that they are interpreted correctly
    /// by the peer.
    fn ack(&mut self, sender: &mut Sender<'_, impl Client>) {
        let window_len = self.rx_window_len();
        let tcp = TcpRepr {
            src_port: sender.ft.dst.port(),
            dst_port: sender.ft.src.port(),
            control: TcpControl::None,
            seq_number: self.tx_send,
            ack_number: Some(self.rx_seq),
            window_len,
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None, None, None],
            timestamp: None,
            payload: &[],
        };

        trace_tcp_packet(sender.ft, &tcp, 0, "ack");

        sender.send_packet(&tcp, None);
        self.stats.standalone_acks_tx.increment();
        self.record_advertised_window(window_len);
    }

    fn handle_listen_syn(
        &mut self,
        sender: &mut Sender<'_, impl Client>,
        tcp: &TcpRepr<'_>,
    ) -> Result<bool, DropReason> {
        if tcp.control != TcpControl::Syn || tcp.segment_len() != 1 {
            tracing::error!(?tcp.control, "invalid packet waiting for syn, drop connection");
            return Ok(false);
        }

        let ack_number = tcp.ack_number.ok_or(TcpError::MissingAck)?;
        if ack_number <= self.tx_acked || ack_number > self.tx_send {
            sender.rst(ack_number, None);
            self.stats.rsts_tx.increment();
            return Ok(false);
        }
        self.tx_acked = ack_number;

        self.initialize_from_first_client_packet(tcp)?;
        self.tx_window_tx_seq = ack_number;
        self.tx_window_len = tcp.window_len;

        // Send an ACK to complete the initial SYN handshake.
        self.ack(sender);

        self.state = TcpState::Established;
        Ok(true)
    }

    fn handle_packet(
        &mut self,
        sender: &mut Sender<'_, impl Client>,
        tcp: &TcpRepr<'_>,
    ) -> Result<bool, DropReason> {
        if self.state == TcpState::Connecting {
            // We have not yet sent a syn (we are still deciding whether we are
            // in LISTEN or CLOSED state), so we can't send a reasonable
            // response to this. Just drop the packet.
            return Err(TcpError::StillConnecting.into());
        } else if self.state == TcpState::SynSent {
            return self.handle_listen_syn(sender, tcp);
        }

        let rx_window_len = self.rx_window_cap - self.rx_buffer.len();
        let rx_window_end = self.rx_seq + rx_window_len;
        let segment_end = tcp.seq_number + tcp.segment_len();

        // Validate the sequence number per RFC 793.
        let seq_acceptable = if rx_window_len != 0 {
            (tcp.seq_number >= self.rx_seq && tcp.seq_number < rx_window_end)
                || (tcp.segment_len() > 0
                    && segment_end > self.rx_seq
                    && segment_end <= rx_window_end)
        } else {
            tcp.segment_len() == 0 && tcp.seq_number == self.rx_seq
        };

        if tcp.control == TcpControl::Rst {
            if !seq_acceptable {
                // Silently drop--don't send an ACK--since the peer would then
                // immediately respond with a valid RST.
                return Err(TcpError::Unacceptable.into());
            }

            // RFC 5961
            if tcp.seq_number != self.rx_seq {
                // Send a challenge ACK.
                self.ack(sender);
                return Ok(true);
            }

            // This is a valid RST. Drop the connection.
            tracing::debug!("connection reset");
            self.last_close_reason = ConnectionCloseReason::PeerRst;
            return Ok(false);
        }

        // Send ack and drop packets with unacceptable sequence numbers.
        if !seq_acceptable {
            self.stats.out_of_window_pkts.increment();
            self.ack(sender);
            return Err(TcpError::Unacceptable.into());
        }

        // SYN should not be set for in-window segments.
        if tcp.control == TcpControl::Syn {
            if self.state == TcpState::SynReceived {
                tracing::debug!("invalid syn, drop connection");
                return Ok(false);
            }
            // RFC 5961, send a challenge ACK.
            self.ack(sender);
            return Ok(true);
        }

        // ACK should always be set at this point.
        let ack_number = tcp.ack_number.ok_or(TcpError::MissingAck)?;

        // FUTURE: validate ack number per RFC 5961.

        // Handle ACK of our SYN.
        if self.state == TcpState::SynReceived {
            if ack_number <= self.tx_acked || ack_number > self.tx_send {
                sender.rst(ack_number, None);
                self.stats.rsts_tx.increment();
                return Ok(false);
            }
            self.tx_window_len = tcp.window_len;
            self.tx_window_rx_seq = tcp.seq_number;
            self.tx_window_tx_seq = ack_number;
            self.tx_acked += 1;
            self.state = TcpState::Established;
        }

        // Ignore ACKs for segments that have not been sent.
        if ack_number > self.tx_send {
            self.ack(sender);
            return Err(TcpError::AckPastSequence.into());
        }

        // Retire the ACKed segments.
        if ack_number > self.tx_acked {
            let mut consumed = ack_number - self.tx_acked;
            if self.tx_fin_buffered && ack_number == self.tx_acked + self.tx_buffer.len() + 1 {
                self.tx_fin_buffered = false;
                consumed -= 1;
                match self.state {
                    TcpState::FinWait1 => self.state = TcpState::FinWait2,
                    TcpState::Closing => self.state = TcpState::TimeWait,
                    TcpState::LastAck => {
                        self.last_close_reason = ConnectionCloseReason::Normal;
                        return Ok(false);
                    }
                    _ => unreachable!(),
                }
            }
            self.tx_buffer.consume(consumed);
            self.tx_acked = ack_number;
        }

        // Update the send window.
        if ack_number >= self.tx_acked
            && (tcp.seq_number > self.tx_window_rx_seq
                || (tcp.seq_number == self.tx_window_rx_seq && ack_number >= self.tx_window_tx_seq))
        {
            self.tx_window_len = tcp.window_len;
            self.tx_window_rx_seq = tcp.seq_number;
            self.tx_window_tx_seq = ack_number;
            // RFC 1323 §2.2: window scaling becomes active after the
            // handshake. The SYN/SYN-ACK window field is unscaled.
            self.tx_window_scale_active = true;
        }

        // Scope the data payload and FIN to the in-window portion of the segment.
        let mut fin = tcp.control == TcpControl::Fin;
        let segment_skip = if tcp.seq_number < self.rx_seq {
            self.rx_seq - tcp.seq_number
        } else {
            0
        };
        let segment_end = if segment_end > rx_window_end {
            fin = false;
            rx_window_end
        } else {
            segment_end
        };
        let payload = &tcp.payload[segment_skip..segment_end - tcp.seq_number - fin as usize];

        let mut rx_fin = false;

        // Process the payload.
        match self.state {
            TcpState::Connecting | TcpState::SynReceived | TcpState::SynSent => unreachable!(),
            TcpState::Established | TcpState::FinWait1 | TcpState::FinWait2 => {
                if !payload.is_empty() || fin {
                    if !payload.is_empty() {
                        self.stats.data_segments_rx_from_guest.increment();
                        self.stats.bytes_rx_from_guest.add(payload.len() as u64);
                        self.stats.rx_segment_size.add_sample(payload.len() as u64);
                    }
                    // Stage 1: Compute the byte offset from the contiguous
                    // frontier.
                    //
                    // Safety of ring_offset: the sequence acceptance check above
                    // bounds the segment to rx_window_end = rx_seq + (rx_window_cap
                    // - rx_buffer.len()), so seq_offset + payload.len() <=
                    // rx_window_cap <= ring capacity.
                    let seq_offset = if tcp.seq_number >= self.rx_seq {
                        tcp.seq_number - self.rx_seq
                    } else {
                        0
                    };
                    let ring_offset = self.rx_buffer.len() + seq_offset;

                    // Stage 2: Record the range in the assembler. Do this
                    // *before* writing to the ring so that rejected segments
                    // (TooManyGaps) don't leave stale bytes in unwritten
                    // ring space.
                    let (rx_consumed, assembler_fin, accepted) =
                        match self
                            .rx_assembler
                            .add(seq_offset as u32, payload.len() as u32, fin)
                        {
                            Ok(result) => (result.consumed as usize, result.fin, true),
                            Err(assembler::TooManyGaps) => (0, false, false),
                        };

                    // Stage 3: Write payload into the ring and advance the
                    // contiguous frontier. Only write when the assembler
                    // accepted the segment.
                    if accepted && !payload.is_empty() {
                        self.rx_buffer.write_at(ring_offset, payload);
                    }
                    self.rx_buffer.extend_by(rx_consumed);
                    self.rx_seq += rx_consumed;
                    rx_fin = assembler_fin;
                    if rx_fin {
                        self.rx_seq += 1;
                    }
                }
                if tcp.segment_len() > 0 {
                    self.needs_ack = true;
                }
            }
            TcpState::CloseWait | TcpState::Closing | TcpState::LastAck => {}
            TcpState::TimeWait => {
                self.ack(sender);
                // TODO: restart timer
            }
        }

        // Process FIN.
        if rx_fin {
            match self.state {
                TcpState::Connecting | TcpState::SynReceived | TcpState::SynSent => unreachable!(),
                TcpState::Established => {
                    self.state = TcpState::CloseWait;
                }
                TcpState::FinWait1 => {
                    self.state = TcpState::Closing;
                }
                TcpState::FinWait2 => {
                    self.state = TcpState::TimeWait;
                    // TODO: start timer
                }
                TcpState::CloseWait
                | TcpState::Closing
                | TcpState::LastAck
                | TcpState::TimeWait => {}
            }
        }

        Ok(true)
    }
}

impl TcpListener {
    /// Creates a `TcpListener` from an already-bound `socket2::Socket`.
    ///
    /// The socket must already be bound to an address. This method will call
    /// `listen` on it.
    pub fn from_socket(driver: &dyn Driver, socket: Socket) -> Result<Self, BindError> {
        let Some(host_port) = socket
            .local_addr()
            .map_err(BindError::Io)?
            .as_socket()
            .map(|addr| addr.port())
        else {
            return Err(BindError::Io(io::Error::other(
                "socket local address is invalid",
            )));
        };
        let socket = PolledSocket::new(driver, socket).map_err(BindError::Io)?;
        if let Err(err) = socket.listen(10) {
            tracing::warn!(
                error = &err as &dyn std::error::Error,
                "socket listen error"
            );
            return Err(BindError::Io(err));
        }
        Ok(Self { socket, host_port })
    }

    fn poll_listener(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Result<Option<(Socket, SocketAddr)>, DropReason> {
        match self.socket.poll_accept(cx) {
            Poll::Ready(r) => match r {
                Ok((socket, address)) => match address.as_socket() {
                    Some(addr) => Ok(Some((socket, addr))),
                    None => {
                        tracing::warn!(?address, "Unknown address from accept");
                        Ok(None)
                    }
                },
                Err(_) => {
                    let err = take_socket_error(&self.socket);
                    tracing::warn!(error = &err as &dyn std::error::Error, "listen failure");
                    Err(DropReason::Io(err))
                }
            },
            Poll::Pending => Ok(None),
        }
    }
}

/// Trace a TCP packet with structured key/value fields.
///
/// Logs protocol-relevant fields (flags, seq, ack, window, payload length)
/// as individual tracing fields instead of dumping the full `TcpRepr` Debug
/// output which includes raw payload bytes.
fn trace_tcp_packet(ft: &FourTuple, tcp: &TcpRepr<'_>, payload_len: usize, label: &str) {
    tracing::trace!(
        label,
        src = %ft.src,
        dst = %ft.dst,
        flags = match tcp.control {
            TcpControl::Syn => Some("SYN"),
            TcpControl::Fin => Some("FIN"),
            TcpControl::Rst => Some("RST"),
            TcpControl::Psh => Some("PSH"),
            TcpControl::None => None,
        },
        seq = tcp.seq_number.0 as u32,
        next_seq = (tcp.seq_number.0 as u32).wrapping_add((payload_len + tcp.control.len()) as u32),
        ack = tcp.ack_number.map(|a| a.0 as u32),
        window = tcp.window_len,
        payload_len,
        "tcp packet",
    );
}

// RFC 1071 primitives vendored from smoltcp (0BSD); replace with a `use` of
// smoltcp::wire::checksum once smoltcp-rs/smoltcp#1172 exposes it.
mod checksum {
    use smoltcp::wire::IpAddress;
    use smoltcp::wire::IpProtocol;

    const fn propagate_carries(word: u32) -> u16 {
        let sum = (word >> 16) + (word & 0xffff);
        ((sum >> 16) as u16) + (sum as u16)
    }

    pub fn data(mut data: &[u8]) -> u16 {
        let mut accum: u32 = 0;
        const CHUNK_SIZE: usize = 32;
        while data.len() >= CHUNK_SIZE {
            let mut d = &data[..CHUNK_SIZE];
            while d.len() >= 2 {
                accum = accum.wrapping_add(u16::from_be_bytes([d[0], d[1]]) as u32);
                d = &d[2..];
            }
            data = &data[CHUNK_SIZE..];
        }
        while data.len() >= 2 {
            accum = accum.wrapping_add(u16::from_be_bytes([data[0], data[1]]) as u32);
            data = &data[2..];
        }
        if let Some(&value) = data.first() {
            accum = accum.wrapping_add((value as u32) << 8);
        }
        propagate_carries(accum)
    }

    pub fn combine(checksums: &[u16]) -> u16 {
        let mut accum: u32 = 0;
        for &word in checksums {
            accum = accum.wrapping_add(word as u32);
        }
        propagate_carries(accum)
    }

    pub fn pseudo_header(
        src_addr: &IpAddress,
        dst_addr: &IpAddress,
        next_header: IpProtocol,
        length: u32,
    ) -> u16 {
        match (src_addr, dst_addr) {
            (IpAddress::Ipv4(src_addr), IpAddress::Ipv4(dst_addr)) => {
                let mut proto_len = [0u8; 4];
                proto_len[1] = next_header.into();
                proto_len[2..4].copy_from_slice(&(length as u16).to_be_bytes());
                combine(&[
                    data(&src_addr.octets()),
                    data(&dst_addr.octets()),
                    data(&proto_len),
                ])
            }
            (IpAddress::Ipv6(src_addr), IpAddress::Ipv6(dst_addr)) => {
                let mut len_proto = [0u8; 8];
                len_proto[0..4].copy_from_slice(&length.to_be_bytes());
                len_proto[7] = next_header.into();
                combine(&[
                    data(&src_addr.octets()),
                    data(&dst_addr.octets()),
                    data(&len_proto),
                ])
            }
            _ => unreachable!("mismatched IP address families"),
        }
    }
}

fn take_socket_error(socket: &PolledSocket<Socket>) -> io::Error {
    match socket.get().take_error() {
        Ok(Some(err)) => err,
        Ok(_) => io::Error::other("missing error"),
        Err(err) => err,
    }
}

/// Log a TCP connect error at the appropriate level.
///
/// Connection refused and network/host unreachable are expected failures logged
/// at debug level. Everything else is logged at warn.
fn log_connect_error(ft: &FourTuple, err: &io::Error) {
    match err.kind() {
        ErrorKind::ConnectionRefused => {
            tracing::debug!(
                error = err as &dyn std::error::Error,
                src = %ft.src,
                dst = %ft.dst,
                "connect refused",
            );
        }
        ErrorKind::NetworkUnreachable | ErrorKind::HostUnreachable => {
            // FUTURE: send ICMP unreachable to guest
            tracing::debug!(
                error = err as &dyn std::error::Error,
                src = %ft.src,
                dst = %ft.dst,
                "connect failed, unreachable",
            );
        }
        _ => {
            tracelimit::warn_ratelimited!(
                error = err as &dyn std::error::Error,
                src = %ft.src,
                dst = %ft.dst,
                "connect failed",
            );
        }
    }
}

fn is_connect_incomplete_error(err: &io::Error) -> bool {
    if err.kind() == ErrorKind::WouldBlock {
        return true;
    }
    // This handles the remaining cases on Linux.
    #[cfg(unix)]
    if err.raw_os_error() == Some(libc::EINPROGRESS) {
        return true;
    }
    false
}

/// Finds the smallest sequence number in a set. To get a coherent result, all
/// the sequence numbers must be known to be comparable, meaning they are all
/// within 2^31 bytes of each other.
///
/// This isn't just `Ord::min` or `Iterator::min` because `TcpSeqNumber`
/// implements `PartialOrd` but not `Ord`.
fn seq_min<const N: usize>(seqs: [TcpSeqNumber; N]) -> TcpSeqNumber {
    let mut min = seqs[0];
    for &seq in &seqs[1..] {
        if min > seq {
            min = seq;
        }
    }
    min
}

/// Check if a TCP connection targets the gateway's DNS port.
fn is_gateway_dns_tcp(ft: &FourTuple, params: &crate::ConsommeParams, dns_available: bool) -> bool {
    if !dns_available || ft.dst.port() != crate::DNS_PORT {
        return false;
    }
    match ft.dst.ip() {
        IpAddr::V4(ip) => params.gateway_ip == ip,
        IpAddr::V6(ip) => params.gateway_link_local_ipv6 == ip,
    }
}

#[cfg(test)]
mod tests;
