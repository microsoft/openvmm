// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod ring;

use super::Access;
use super::Client;
use super::DropReason;
use crate::ChecksumState;
use crate::ConsommeState;
use crate::IpAddresses;
use futures::AsyncRead;
use futures::AsyncWrite;
use inspect::Inspect;
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
use std::collections::VecDeque;
use std::collections::hash_map;
use std::io;
use std::io::ErrorKind;
use std::io::IoSlice;
use std::io::IoSliceMut;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
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

pub(crate) struct Tcp {
    connections: HashMap<FourTuple, TcpConnection>,
    listeners: HashMap<u16, TcpListener>,
}

#[derive(Debug, Error)]
pub enum TcpError {
    #[error("still connecting")]
    StillConnecting,
    #[error("unacceptable segment number")]
    Unacceptable,
    #[error("received out of order packet")]
    OutOfOrder,
    #[error("missing ack bit")]
    MissingAck,
    #[error("ack newer than sequence")]
    AckPastSequence,
    #[error("invalid window scale")]
    InvalidWindowScale,
}

impl Inspect for Tcp {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        for (addr, conn) in &self.connections {
            resp.field(
                &format!(
                    "{}:{}-{}:{}",
                    addr.src.ip(),
                    addr.src.port(),
                    addr.dst.ip(),
                    addr.dst.port()
                ),
                conn,
            );
        }
        for port in self.listeners.keys() {
            resp.field("listening port", port);
        }
    }
}

impl Tcp {
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
            listeners: HashMap::new(),
        }
    }
}

#[derive(Inspect)]
#[inspect(tag = "info")]
enum LoopbackPortInfo {
    None,
    ProxyForGuestPort { sending_port: u16, guest_port: u16 },
}

#[derive(Inspect)]
struct TcpConnection {
    #[inspect(skip)]
    socket: Option<PolledSocket<Socket>>,
    loopback_port: LoopbackPortInfo,
    state: TcpState,

    #[inspect(with = "|x| x.len()")]
    rx_buffer: VecDeque<u8>,
    #[inspect(hex)]
    rx_window_cap: usize,
    rx_window_scale: u8,
    #[inspect(with = "inspect_seq")]
    rx_seq: TcpSeqNumber,
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
                                if connection.state == TcpState::Connecting && other_ft.dst.port() == *port {
                                    if let LoopbackPortInfo::ProxyForGuestPort{sending_port, guest_port} = connection.loopback_port {
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
                                src: SocketAddr::V4(SocketAddrV4::new(self.inner.state.params.client_ip.into(), *port)),
                            },
                            SocketAddr::V6(_) => {
                                let client_ipv6 = match self.inner.state.params.client_ip_ipv6 {
                                    Some(ip) => ip.into(),
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
            conn.poll_conn(
                cx,
                &mut Sender {
                    ft,
                    state: &mut self.inner.state,
                    client: self.client,
                },
            )
        })
    }

    pub(crate) fn refresh_tcp_driver(&mut self) {
        self.inner.tcp.connections.retain(|_, conn| {
            let Some(socket) = conn.socket.take() else {
                return true;
            };
            let socket = socket.into_inner();
            match PolledSocket::new(self.client.driver(), socket) {
                Ok(socket) => {
                    conn.socket = Some(socket);
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
                dst: SocketAddr::V4(SocketAddrV4::new(addresses.dst_addr.into(), tcp.dst_port)),
                src: SocketAddr::V4(SocketAddrV4::new(addresses.src_addr.into(), tcp.src_port)),
            },
            IpAddresses::V6(addresses) => FourTuple {
                dst: SocketAddr::V6(SocketAddrV6::new(
                    addresses.dst_addr.into(),
                    tcp.dst_port,
                    0,
                    0,
                )),
                src: SocketAddr::V6(SocketAddrV6::new(
                    addresses.src_addr.into(),
                    tcp.src_port,
                    0,
                    0,
                )),
            },
        };
        tracing::trace!(?tcp, "tcp packet");

        let mut sender = Sender {
            ft: &ft,
            client: self.client,
            state: &mut self.inner.state,
        };

        match self.inner.tcp.connections.entry(ft) {
            hash_map::Entry::Occupied(mut e) => {
                let conn = e.get_mut();
                if !conn.handle_packet(&mut sender, &tcp)? {
                    e.remove();
                }
            }
            hash_map::Entry::Vacant(e) => {
                if tcp.control == TcpControl::Rst {
                    // This connection is already closed. Ignore the packet.
                } else if let Some(ack) = tcp.ack_number {
                    // This is for an old connection. Send reset.
                    sender.rst(ack, None);
                } else if tcp.control == TcpControl::Syn {
                    let conn = TcpConnection::new(&mut sender, &tcp)?;
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
    pub fn bind_tcp_port(&mut self, ip_addr: Option<IpAddr>, port: u16) -> Result<(), DropReason> {
        let ip_addr = match ip_addr {
            Some(IpAddr::V4(ip)) => SocketAddr::V4(SocketAddrV4::new(ip, port)),
            Some(IpAddr::V6(ip)) => SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0)),
            None => SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)),
        };
        match self.inner.tcp.listeners.entry(port) {
            hash_map::Entry::Occupied(_) => {
                tracing::warn!(port, "Duplicate TCP bind for port");
            }
            hash_map::Entry::Vacant(e) => {
                let ft = match ip_addr {
                    SocketAddr::V4(ip) => FourTuple {
                        dst: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)),
                        src: SocketAddr::V4(ip),
                    },
                    SocketAddr::V6(ip) => FourTuple {
                        dst: SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0)),
                        src: SocketAddr::V6(ip),
                    },
                };
                let mut sender = Sender {
                    ft: &ft,
                    client: self.client,
                    state: &mut self.inner.state,
                };

                let listener = TcpListener::new(&mut sender)?;
                e.insert(listener);
            }
        }
        Ok(())
    }

    /// Unbinds from the specified host port.
    pub fn unbind_tcp_port(&mut self, port: u16) -> Result<(), DropReason> {
        match self.inner.tcp.listeners.entry(port) {
            hash_map::Entry::Occupied(e) => {
                e.remove();
                Ok(())
            }
            hash_map::Entry::Vacant(_) => Err(DropReason::PortNotBound),
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
        let copy_payload_into_buffer = |buf: &mut [u8], payload: Option<ring::View<'_>>| {
            if let Some(payload) = payload {
                for (b, c) in buf.iter_mut().zip(payload.iter()) {
                    *b = *c;
                }
            }
        };
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
                (&mut ip_packet_buf[payload_offset..], total_len)
            }
            SocketAddr::V6(_) => {
                let ipv6_packet = Ipv6Packet::new_unchecked(&*ip_packet_buf);
                let total_len = ipv6_packet.total_len();
                let payload_offset = IPV6_HEADER_LEN;
                (&mut ip_packet_buf[payload_offset..], total_len)
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
        copy_payload_into_buffer(tcp_packet.payload_mut(), payload);
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
            payload: &[],
        };

        tracing::trace!(?tcp, "tcp rst xmit");

        self.send_packet(&tcp, None);
    }
}

impl Default for TcpConnection {
    fn default() -> Self {
        let mut rx_tx_seq = [0; 8];
        getrandom::fill(&mut rx_tx_seq[..]).expect("prng failure");
        let rx_seq = TcpSeqNumber(i32::from_ne_bytes(
            rx_tx_seq[0..4].try_into().expect("invalid length"),
        ));
        let tx_seq = TcpSeqNumber(i32::from_ne_bytes(
            rx_tx_seq[4..8].try_into().expect("invalid length"),
        ));

        let rx_buffer_size: usize = 16384;
        let rx_window_scale =
            (usize::BITS - rx_buffer_size.leading_zeros()).saturating_sub(16) as u8;

        let tx_buffer_size = 16384;

        Self {
            socket: None,
            loopback_port: LoopbackPortInfo::None,
            state: TcpState::Connecting,
            rx_buffer: VecDeque::with_capacity(rx_buffer_size),
            rx_window_cap: 0,
            rx_window_scale,
            rx_seq,
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
}

impl TcpConnection {
    fn new(sender: &mut Sender<'_, impl Client>, tcp: &TcpRepr<'_>) -> Result<Self, DropReason> {
        let mut this = Self::default();
        this.initialize_from_first_client_packet(tcp)?;

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
                tracing::warn!(
                    error = &err as &dyn std::error::Error,
                    "socket connect error"
                );
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
                        this.loopback_port = LoopbackPortInfo::ProxyForGuestPort {
                            sending_port: addr.port(),
                            guest_port: sender.ft.src.port(),
                        };
                    }
                }
            }
        }
        this.socket = Some(socket);
        Ok(this)
    }

    fn new_from_accept(
        sender: &mut Sender<'_, impl Client>,
        socket: Socket,
    ) -> Result<Self, DropReason> {
        let mut this = Self {
            socket: Some(
                PolledSocket::new(sender.client.driver(), socket).map_err(DropReason::Io)?,
            ),
            state: TcpState::SynSent,
            ..Default::default()
        };
        this.send_syn(sender, None);
        Ok(this)
    }

    fn initialize_from_first_client_packet(&mut self, tcp: &TcpRepr<'_>) -> Result<(), DropReason> {
        // The TCPv4 default maximum segment size is 536. This can be bigger for
        // IPv6.
        let tx_mss = tcp.max_seg_size.map_or(536, |x| x.into());

        if let Some(tx_window_scale) = tcp.window_scale {
            if tx_window_scale > 14 {
                return Err(TcpError::InvalidWindowScale.into());
            }
        }

        let max_rx_buffer_size = if tcp.window_scale.is_some() {
            u32::MAX as usize
        } else {
            u16::MAX as usize
        };
        let rx_buffer_size = 16384.min(max_rx_buffer_size);
        let rx_window_scale =
            (usize::BITS - rx_buffer_size.leading_zeros()).saturating_sub(16) as u8;

        assert!(tcp.window_scale.is_some() || rx_window_scale == 0);
        if self.rx_buffer.capacity() < rx_buffer_size {
            self.rx_buffer.reserve_exact(rx_buffer_size);
        }

        self.rx_window_scale = rx_window_scale;
        self.rx_seq = tcp.seq_number + 1;
        self.tx_window_rx_seq = tcp.seq_number + 1;
        self.enable_window_scaling = tcp.window_scale.is_some();
        self.tx_window_scale = tcp.window_scale.unwrap_or(0);
        self.tx_mss = tx_mss;
        Ok(())
    }

    fn poll_conn(&mut self, cx: &mut Context<'_>, sender: &mut Sender<'_, impl Client>) -> bool {
        if self.state == TcpState::Connecting {
            match self
                .socket
                .as_mut()
                .unwrap()
                .poll_ready(cx, PollEvents::OUT)
            {
                Poll::Ready(r) => {
                    if r.has_err() {
                        let err = take_socket_error(self.socket.as_mut().unwrap());
                        let reset = match err.kind() {
                            ErrorKind::TimedOut => {
                                // Avoid resetting so that the guest doesn't
                                // think there is a responding TCP stack at this
                                // address. The guest will time out on its own.
                                tracing::debug!(
                                    error = &err as &dyn std::error::Error,
                                    "connect timed out"
                                );
                                false
                            }
                            ErrorKind::ConnectionRefused => {
                                // Presumably the remote TCP stack send a RST.
                                // Send a reset but don't log anything.
                                tracing::debug!(
                                    error = &err as &dyn std::error::Error,
                                    "connection refused"
                                );
                                true
                            }
                            _ => {
                                // Something unexpected happened. Log and reset.
                                //
                                // FUTURE: Handle more cases, especially
                                // ENETUNREACH and similar, once we figure out
                                // the right behavior for these. They might
                                // require sending ICMP packets.
                                tracing::warn!(
                                    error = &err as &dyn std::error::Error,
                                    "unhandled connect failure"
                                );
                                true
                            }
                        };
                        if reset {
                            sender.rst(self.tx_send, Some(self.rx_seq));
                        }
                        return false;
                    }

                    tracing::debug!("connection established");
                    self.state = TcpState::SynReceived;
                    self.rx_window_cap = self.rx_buffer.capacity();
                }
                Poll::Pending => return true,
            }
        } else if self.state == TcpState::SynSent {
            // Need to establish connection with client before sending data.
            return true;
        }

        // Handle the tx path.
        if self.socket.is_some() {
            if self.state.tx_fin() {
                if let Poll::Ready(events) = self
                    .socket
                    .as_mut()
                    .unwrap()
                    .poll_ready(cx, PollEvents::EMPTY)
                {
                    if events.has_err() {
                        let err = take_socket_error(self.socket.as_ref().unwrap());
                        match err.kind() {
                            ErrorKind::BrokenPipe | ErrorKind::ConnectionReset => {}
                            _ => tracing::warn!(
                                error = &err as &dyn std::error::Error,
                                "socket failure after fin"
                            ),
                        }
                        sender.rst(self.tx_send, Some(self.rx_seq));
                        return false;
                    }

                    // Both ends are closed. Close the actual socket.
                    self.socket = None;
                }
            } else {
                while !self.tx_buffer.is_full() {
                    let (a, b) = self.tx_buffer.unwritten_slices_mut();
                    let mut bufs = [IoSliceMut::new(a), IoSliceMut::new(b)];
                    match Pin::new(&mut *self.socket.as_mut().unwrap())
                        .poll_read_vectored(cx, &mut bufs)
                    {
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
                                _ => tracing::warn!(
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
        if self.socket.is_some() {
            while !self.rx_buffer.is_empty() {
                let (a, b) = self.rx_buffer.as_slices();
                let bufs = [IoSlice::new(a), IoSlice::new(b)];
                match Pin::new(&mut *self.socket.as_mut().unwrap()).poll_write_vectored(cx, &bufs) {
                    Poll::Ready(Ok(n)) => {
                        self.rx_buffer.drain(..n);
                    }
                    Poll::Ready(Err(err)) => {
                        match err.kind() {
                            ErrorKind::BrokenPipe | ErrorKind::ConnectionReset => {}
                            _ => {
                                tracing::warn!(
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
                if let Err(err) = self
                    .socket
                    .as_ref()
                    .unwrap()
                    .get()
                    .shutdown(Shutdown::Write)
                {
                    tracing::warn!(error = &err as &dyn std::error::Error, "shutdown error");
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
            window_len: self.rx_window_len(),
            window_scale,
            max_seg_size: Some(max_seg_size),
            sack_permitted: false,
            sack_ranges: [None, None, None],
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

            tracing::trace!(?tcp, %tx_next, "tcp xmit");

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
            payload: &[],
        };

        tracing::trace!(?tcp, "tcp ack xmit");

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
        self.rx_window_cap = self.rx_buffer.capacity();
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

        // Also ack+drop for out-of-order non-empty segments rather than queueing
        // them. Our environment makes out-of-order segments unlikely.
        if tcp.seq_number > self.rx_seq && tcp.segment_len() > 0 {
            self.ack(sender);
            return Err(TcpError::OutOfOrder.into());
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

        // Process the payload.
        match self.state {
            TcpState::Connecting | TcpState::SynReceived | TcpState::SynSent => unreachable!(),
            TcpState::Established | TcpState::FinWait1 | TcpState::FinWait2 => {
                self.rx_buffer.extend(payload);
                self.rx_seq = segment_end;
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
        if fin {
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
    pub fn new(sender: &mut Sender<'_, impl Client>) -> Result<Self, DropReason> {
        let socket = match sender.ft.src {
            SocketAddr::V4(_) => Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)),
            SocketAddr::V6(_) => Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP)),
        }
        .map_err(DropReason::Io)?;

        let socket = PolledSocket::new(sender.client.driver(), socket).map_err(DropReason::Io)?;
        if let Err(err) = socket.get().bind(&sender.ft.src.into()) {
            tracing::warn!(
                address = ?sender.ft.src,
                error = &err as &dyn std::error::Error,
                "socket bind error"
            );
            return Err(DropReason::Io(err));
        }
        if let Err(err) = socket.listen(10) {
            tracing::warn!(
                error = &err as &dyn std::error::Error,
                "socket listen error"
            );
            return Err(DropReason::Io(err));
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

fn take_socket_error(socket: &PolledSocket<Socket>) -> io::Error {
    match socket.get().take_error() {
        Ok(Some(err)) => err,
        Ok(_) => io::Error::other("missing error"),
        Err(err) => err,
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
