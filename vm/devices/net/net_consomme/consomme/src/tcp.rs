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
use crate::IpAddresses;
use crate::dns_resolver::DnsResolver;
use crate::dns_resolver::dns_tcp::DnsTcpHandler;
use futures::AsyncRead;
use futures::AsyncWrite;
use inspect::Inspect;
use inspect::InspectMut;
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

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
struct FourTuple {
    src: SocketAddr,
    dst: SocketAddr,
}

impl core::fmt::Display for FourTuple {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}-{}", self.src, self.dst)
    }
}

#[derive(InspectMut)]
pub(crate) struct Tcp {
    #[inspect(iter_by_key)]
    connections: HashMap<FourTuple, TcpConnection>,
    #[inspect(iter_by_key)]
    listeners: HashMap<u16, TcpListener>,
    #[inspect(mut)]
    connection_params: ConnectionParams,
}

#[derive(InspectMut)]
struct ConnectionParams {
    #[inspect(mut)]
    rx_buffer_size: usize,
    #[inspect(mut)]
    tx_buffer_size: usize,
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
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
            listeners: HashMap::new(),
            connection_params: ConnectionParams {
                rx_buffer_size: 256 * 1024,
                tx_buffer_size: 256 * 1024,
            },
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
    #[inspect(with = "inspect_seq")]
    rx_seq: TcpSeqNumber,
    #[inspect(flatten)]
    rx_assembler: assembler::Assembler,
    needs_ack: bool,
    is_shutdown: bool,
    enable_window_scaling: bool,

    #[inspect(with = "|x| x.len()")]
    tx_buffer: ring::Ring,
    #[inspect(with = "inspect_seq")]
    tx_acked: TcpSeqNumber,
    #[inspect(with = "inspect_seq")]
    tx_send: TcpSeqNumber,
    tx_fin_buffered: bool,
    #[inspect(hex)]
    tx_window_len: u16,
    tx_window_scale: u8,
    #[inspect(with = "inspect_seq")]
    tx_window_rx_seq: TcpSeqNumber,
    #[inspect(with = "inspect_seq")]
    tx_window_tx_seq: TcpSeqNumber,
    #[inspect(hex)]
    tx_mss: usize,
}

fn inspect_seq(seq: &TcpSeqNumber) -> inspect::AsHex<u32> {
    inspect::AsHex(seq.0 as u32)
}

#[derive(Inspect)]
struct TcpListener {
    #[inspect(skip)]
    socket: PolledSocket<Socket>,
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
            .retain(|port, listener| match listener.poll_listener(cx) {
                Ok(result) => {
                    if let Some((socket, mut other_addr)) = result {
                        // Check for loopback requests and replace the dest port.
                        // This supports a guest owning both the sending and receiving ports.
                        if other_addr.ip().is_loopback() {
                            for (other_ft, connection) in self.inner.tcp.connections.iter() {
                                if connection.inner.state == TcpState::Connecting && other_ft.dst.port() == *port {
                                    if let LoopbackPortInfo::ProxyForGuestPort{sending_port, guest_port} = connection.inner.loopback_port {
                                        if sending_port == other_addr.port() {
                                            other_addr.set_port(guest_port);
                                            break;
                                        }
                                    }
                                }
                            }
                        }

                        let ft = match other_addr {
                            SocketAddr::V4(_) => FourTuple {
                                dst: other_addr,
                                src: SocketAddr::V4(SocketAddrV4::new(self.inner.state.params.client_ip, *port)),
                            },
                            SocketAddr::V6(_) => {
                                let client_ipv6 = match self.inner.state.params.client_ip_ipv6 {
                                    Some(ip) => ip,
                                    None => {
                                        tracing::warn!("Received IPv6 connection but client IPv6 address is not known");
                                        return true;
                                    }
                                };
                                FourTuple {
                                    dst: other_addr,
                                    src: SocketAddr::V6(SocketAddrV6::new(client_ipv6, *port, 0, 0)),
                                }
                            }
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
                                        tracing::warn!(err = %err, "Failed to create connection from newly accepted socket");
                                        return true;
                                    }
                                };
                                e.insert(conn);
                            }
                            hash_map::Entry::Occupied(_) => {
                                tracing::warn!(
                                    address = ?ft.dst,
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
            match &mut conn.backend {
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
            }
        })
    }

    pub(crate) fn refresh_tcp_driver(&mut self) {
        self.inner.tcp.connections.retain(|_, conn| {
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
                if !keep {
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
                        TcpConnection::new(&mut sender, &tcp, &self.inner.tcp.connection_params)?
                    };
                    e.insert(conn);
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
        match self.inner.tcp.listeners.entry(guest_port) {
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

    /// Unbinds from the specified host port.
    pub fn unbind_tcp_port(&mut self, port: u16) -> Result<(), BindError> {
        match self.inner.tcp.listeners.entry(port) {
            hash_map::Entry::Occupied(e) => {
                e.remove();
                Ok(())
            }
            hash_map::Entry::Vacant(_) => Err(BindError::PortNotBound),
        }
    }
}

struct Sender<'a, T> {
    ft: &'a FourTuple,
    client: &'a mut T,
    state: &'a mut ConsommeState,
}

impl<T: Client> Sender<'_, T> {
    fn send_packet(&mut self, tcp: &TcpRepr<'_>, payload: Option<ring::View<'_>>) {
        let buffer = &mut self.state.buffer;
        let mut eth_packet = EthernetFrame::new_unchecked(&mut buffer[..]);
        eth_packet.set_dst_addr(self.state.params.client_mac);
        eth_packet.set_src_addr(self.state.params.gateway_mac);
        let ip = IpRepr::new(
            self.ft.dst.ip().into(),
            self.ft.src.ip().into(),
            IpProtocol::Tcp,
            tcp.header_len() + payload.as_ref().map_or(0, |p| p.len()),
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
        tcp.emit(
            &mut tcp_packet,
            &dst_ip_addr,
            &src_ip_addr,
            &ChecksumCapabilities::default(),
        );

        // Copy payload into TCP packet
        if let Some(payload) = &payload {
            payload.copy_to_slice(tcp_packet.payload_mut());
        }
        tcp_packet.fill_checksum(&self.ft.dst.ip().into(), &self.ft.src.ip().into());
        let n = ETHERNET_HEADER_LEN + ip_total_len;
        let checksum_state = match self.ft.dst {
            SocketAddr::V4(_) => ChecksumState::TCP4,
            SocketAddr::V6(_) => ChecksumState::TCP6,
        };

        self.client.recv(&buffer[..n], &checksum_state);
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

        let rx_buffer_size: usize = params.rx_buffer_size.clamp(16384, 4 << 20);
        let rx_window_scale =
            (usize::BITS - rx_buffer_size.leading_zeros()).saturating_sub(16) as u8;

        let tx_buffer_size = params
            .tx_buffer_size
            .clamp(16384, 4 << 20)
            .next_power_of_two();

        TcpConnectionInner {
            loopback_port: LoopbackPortInfo::None,
            state: TcpState::Connecting,
            rx_buffer: ring::Ring::new(0),
            rx_window_cap: rx_buffer_size,
            rx_window_scale,
            rx_seq,
            rx_assembler: assembler::Assembler::new(),
            needs_ack: false,
            is_shutdown: false,
            enable_window_scaling: false,
            tx_buffer: ring::Ring::new(tx_buffer_size),
            tx_acked: tx_seq,
            tx_send: tx_seq,
            tx_window_len: 1,
            tx_window_scale: 0,
            tx_window_rx_seq: rx_seq,
            tx_window_tx_seq: tx_seq,
            // The TCPv4 default maximum segment size is 536. This can be bigger for
            // IPv6.
            tx_mss: 536,
            tx_fin_buffered: false,
        }
    }

    fn new(
        sender: &mut Sender<'_, impl Client>,
        tcp: &TcpRepr<'_>,
        params: &ConnectionParams,
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
                log_connect_error(&err);
                sender.rst(TcpSeqNumber(0), Some(tcp.seq_number + tcp.segment_len()));
                return Err(DropReason::Io(err));
            }
        }
        if let Ok(addr) = socket.get().local_addr() {
            match addr.as_socket() {
                None => {
                    tracing::warn!("unable to get local socket address");
                }
                Some(addr) => {
                    if addr.ip().is_loopback() {
                        inner.loopback_port = LoopbackPortInfo::ProxyForGuestPort {
                            sending_port: addr.port(),
                            guest_port: sender.ft.src.port(),
                        };
                    }
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
        let mut inner = TcpConnectionInner {
            state: TcpState::SynSent,
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
                return false;
            }
        }

        self.send_next(sender);
        !(self.state == TcpState::TimeWait
            || self.state == TcpState::LastAck
            || (self.state.tx_fin() && self.state.rx_fin() && self.tx_buffer.is_empty()))
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
                                "socket failure after fin"
                            ),
                        }
                        sender.rst(self.tx_send, Some(self.rx_seq));
                        return false;
                    }

                    // Both ends are closed. Close the actual socket.
                    *opt_socket = None;
                }
            } else {
                while !self.tx_buffer.is_full() {
                    let (a, b) = self.tx_buffer.unwritten_slices_mut();
                    let mut bufs = [IoSliceMut::new(a), IoSliceMut::new(b)];
                    match Pin::new(&mut *socket).poll_read_vectored(cx, &mut bufs) {
                        Poll::Ready(Ok(n)) => {
                            if n == 0 {
                                self.close();
                                break;
                            }
                            self.tx_buffer.extend_by(n);
                        }
                        Poll::Ready(Err(err)) => {
                            match err.kind() {
                                ErrorKind::ConnectionReset => tracing::trace!(
                                    error = &err as &dyn std::error::Error,
                                    "socket read error"
                                ),
                                _ => tracelimit::warn_ratelimited!(
                                    error = &err as &dyn std::error::Error,
                                    "socket read error"
                                ),
                            }
                            sender.rst(self.tx_send, Some(self.rx_seq));
                            return false;
                        }
                        Poll::Pending => break,
                    }
                }
            }
        }

        // Handle the rx path.
        if let Some(socket) = opt_socket.as_mut() {
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
                                    "socket write error"
                                );
                            }
                        }
                        sender.rst(self.tx_send, Some(self.rx_seq));
                        return false;
                    }
                    Poll::Pending => break,
                }
            }
            if self.rx_buffer.is_empty() && self.state.rx_fin() && !self.is_shutdown {
                if let Err(err) = socket.get().shutdown(Shutdown::Write) {
                    tracelimit::warn_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        "shutdown error"
                    );
                    sender.rst(self.tx_send, Some(self.rx_seq));
                    return false;
                }
                self.is_shutdown = true;
            }
        }

        // Send whatever needs to be sent.
        self.send_next(sender);
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
            log_connect_error(&err);
            sender.rst(self.tx_send, Some(self.rx_seq));
        }
    }

    fn rx_window_len(&self) -> u16 {
        ((self.rx_window_cap - self.rx_buffer.len()) >> self.rx_window_scale) as u16
    }

    fn send_next(&mut self, sender: &mut Sender<'_, impl Client>) {
        match self.state {
            TcpState::Connecting => {}
            TcpState::SynReceived => self.send_syn(sender, Some(self.rx_seq)),
            _ => self.send_data(sender),
        }
    }

    fn send_syn(&mut self, sender: &mut Sender<'_, impl Client>, ack_number: Option<TcpSeqNumber>) {
        if self.tx_send != self.tx_acked || sender.client.rx_mtu() == 0 {
            return;
        }

        // If the client side specified a window scale option, then do the same
        // (even with no shift) to enable window scale support.
        let window_scale = self.enable_window_scaling.then_some(self.rx_window_scale);

        // Advertise the maximum possible segment size, allowing the guest
        // to truncate this to its own MTU calculation.
        let max_seg_size = u16::MAX;
        let tcp = TcpRepr {
            src_port: sender.ft.dst.port(),
            dst_port: sender.ft.src.port(),
            control: TcpControl::Syn,
            seq_number: self.tx_send,
            ack_number,
            window_len: if ack_number.is_some() {
                self.rx_window_len()
            } else {
                0
            },
            window_scale,
            max_seg_size: Some(max_seg_size),
            sack_permitted: false,
            sack_ranges: [None, None, None],
            timestamp: None,
            payload: &[],
        };

        sender.send_packet(&tcp, None);
        self.tx_send += 1;
    }

    fn send_data(&mut self, sender: &mut Sender<'_, impl Client>) {
        // These computations assume syn has already been sent and acked.
        let tx_payload_end = self.tx_acked + self.tx_buffer.len();
        let tx_end = tx_payload_end + self.tx_fin_buffered as usize;
        let tx_window_end = self.tx_acked + ((self.tx_window_len as usize) << self.tx_window_scale);
        let tx_done = seq_min([tx_end, tx_window_end]);

        while self.needs_ack || self.tx_send < tx_done {
            let rx_mtu = sender.client.rx_mtu();
            if rx_mtu == 0 {
                // Out of receive buffers.
                break;
            }

            let mut tcp = TcpRepr {
                src_port: sender.ft.dst.port(),
                dst_port: sender.ft.src.port(),
                control: TcpControl::None,
                seq_number: self.tx_send,
                ack_number: Some(self.rx_seq),
                window_len: self.rx_window_len(),
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

            // Include the fin if present if there is still room.
            if self.tx_fin_buffered
                && tcp.control == TcpControl::None
                && tx_next == tx_payload_end
                && tx_next < tx_window_end
            {
                tcp.control = TcpControl::Fin;
                tx_next += 1;
            }

            assert!(tx_next <= tx_end);
            assert!(self.needs_ack || tx_next > self.tx_send);

            trace_tcp_packet(sender.ft, &tcp, payload_len, "xmit");

            let payload = self
                .tx_buffer
                .view(payload_start..payload_start + payload_len);

            sender.send_packet(&tcp, Some(payload));
            self.tx_send = tx_next;
            self.needs_ack = false;
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
    fn ack(&self, sender: &mut Sender<'_, impl Client>) {
        let tcp = TcpRepr {
            src_port: sender.ft.dst.port(),
            dst_port: sender.ft.src.port(),
            control: TcpControl::None,
            seq_number: self.tx_send,
            ack_number: Some(self.rx_seq),
            window_len: self.rx_window_len(),
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None, None, None],
            timestamp: None,
            payload: &[],
        };

        trace_tcp_packet(sender.ft, &tcp, 0, "ack");

        sender.send_packet(&tcp, None);
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
            return Ok(false);
        }

        // Send ack and drop packets with unacceptable sequence numbers.
        if !seq_acceptable {
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
                    TcpState::LastAck => return Ok(false),
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
        let socket = PolledSocket::new(driver, socket).map_err(BindError::Io)?;
        if let Err(err) = socket.listen(10) {
            tracing::warn!(
                error = &err as &dyn std::error::Error,
                "socket listen error"
            );
            return Err(BindError::Io(err));
        }
        Ok(Self { socket })
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
fn log_connect_error(err: &io::Error) {
    match err.kind() {
        ErrorKind::ConnectionRefused => {
            tracing::debug!(error = err as &dyn std::error::Error, "connect refused");
        }
        ErrorKind::NetworkUnreachable | ErrorKind::HostUnreachable => {
            // FUTURE: send ICMP unreachable to guest
            tracing::debug!(
                error = err as &dyn std::error::Error,
                "connect failed, unreachable"
            );
        }
        _ => {
            tracelimit::warn_ratelimited!(error = err as &dyn std::error::Error, "connect failed");
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
