// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The relay of the guest's IPv4 ICMP echo requests, which carry ping.
//!
//! The gateway answers the echo requests that are addressed to it itself. The
//! relay sends the guest's other echo requests through a host ICMP socket, one
//! per guest address, and returns the echo replies that answer them. It
//! relays nothing else: the guest's other ICMP messages are dropped, and so are
//! the host's ICMP messages that answer no request of the flow, which a raw
//! socket receives for the whole host. ICMP errors, such as an unreachable
//! destination or an expired hop limit, do not reach the guest.
//!
//! The host's socket is a raw socket on Windows, and an unprivileged datagram
//! socket elsewhere, which needs the group of the process to be in the host's
//! `net.ipv4.ping_group_range` on Linux. A host that refuses the socket drops
//! the request, and the guest sees no reply. The [`echo`] module describes how
//! replies find their requests.

// UNSAFETY: needed to cast the socket buffer to `MaybeUninit`.
#![expect(unsafe_code)]

mod echo;

use self::echo::Delivery;
use self::echo::PendingEchoes;
use super::Access;
use super::Client;
use super::ConsommeState;
use super::DropReason;
use crate::ChecksumState;
use crate::Ipv4Addresses;
use crate::MIN_MTU;
use crate::limits::FlowProtocol;

use inspect::Inspect;
use inspect_counters::Counter;
use pal_async::interest::InterestSlot;
use pal_async::interest::PollEvents;
use pal_async::socket::PolledSocket;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::ETHERNET_HEADER_LEN;
use smoltcp::wire::EthernetAddress;
use smoltcp::wire::EthernetFrame;
use smoltcp::wire::EthernetProtocol;
use smoltcp::wire::EthernetRepr;
use smoltcp::wire::IPV4_HEADER_LEN;
use smoltcp::wire::Icmpv4Message;
use smoltcp::wire::Icmpv4Packet;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Packet;
use smoltcp::wire::Ipv4Repr;
use socket2::Domain;
use socket2::Protocol;
use socket2::SockAddr;
use socket2::Socket;
use socket2::Type;
use std::collections::HashMap;
use std::collections::hash_map;
use std::io::ErrorKind;
use std::mem::MaybeUninit;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::net::SocketAddrV4;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use std::time::Instant;

const ICMPV4_HEADER_LEN: usize = 8;

pub(crate) struct Icmp {
    connections: HashMap<SocketAddrV4, IcmpConnection>,
    timeout: Duration,
    max_connections: Option<usize>,
}

impl Icmp {
    pub fn new(timeout: Duration, max_connections: Option<usize>) -> Self {
        Self {
            connections: HashMap::new(),
            timeout,
            max_connections,
        }
    }

    /// Rejects a new guest source flow when the active-flow limit is reached,
    /// before a host socket is opened for it.
    fn check_flow_limit(&self, guest_addr: &SocketAddrV4) -> Result<(), DropReason> {
        crate::limits::check_flow_limit(
            FlowProtocol::Icmp,
            &self.connections,
            self.max_connections,
            guest_addr,
        )
    }
}

impl Inspect for Icmp {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        resp.field("max_connections", self.max_connections)
            .field("active_connections", self.connections.len());
        for (addr, conn) in &self.connections {
            resp.field(&format!("{}:{}", addr.ip(), addr.port()), conn);
        }
    }
}

#[derive(Inspect)]
struct IcmpConnection {
    #[inspect(skip)]
    socket: Option<PolledSocket<Socket>>,
    #[inspect(display)]
    guest_mac: EthernetAddress,
    #[inspect(with = "|x| x.len()")]
    echoes: PendingEchoes,
    stats: Stats,
    #[inspect(debug)]
    last_activity: Instant,
}

#[derive(Inspect, Default)]
struct Stats {
    tx_packets: Counter,
    tx_dropped: Counter,
    tx_errors: Counter,
    rx_packets: Counter,
    /// Messages from the host's socket that were no reply to a request of the
    /// flow, or were malformed.
    rx_dropped: Counter,
}

impl IcmpConnection {
    fn poll_conn(
        &mut self,
        cx: &mut Context<'_>,
        dst_addr: &SocketAddrV4,
        state: &mut ConsommeState,
        client: &mut impl Client,
    ) -> bool {
        let mut eth = EthernetFrame::new_unchecked(&mut state.buffer);
        loop {
            // The message lands after the room that the guest's IPv4 header
            // will take, which spares moving it.
            match self.socket.as_mut().unwrap().poll_io(
                cx,
                InterestSlot::Read,
                PollEvents::IN,
                |socket| Self::recv_from(socket.get(), &mut eth.payload_mut()[IPV4_HEADER_LEN..]),
            ) {
                Poll::Ready(Ok((n, peer))) => {
                    let peer = peer.as_socket_ipv4().map(|peer| *peer.ip());
                    let Some(reply) = self.echoes.relay_reply(
                        Delivery::HOST,
                        &mut eth.payload_mut()[IPV4_HEADER_LEN..],
                        n,
                        peer,
                        Instant::now(),
                    ) else {
                        self.stats.rx_dropped.increment();
                        continue;
                    };

                    // Add the IPv4 header and the Ethernet frame around the
                    // message that was received.
                    eth.set_ethertype(EthernetProtocol::Ipv4);
                    eth.set_src_addr(state.params.gateway_mac);
                    eth.set_dst_addr(self.guest_mac);
                    let mut ipv4 = Ipv4Packet::new_unchecked(eth.payload_mut());
                    Ipv4Repr {
                        src_addr: reply.source,
                        dst_addr: *dst_addr.ip(),
                        next_header: IpProtocol::Icmp,
                        payload_len: reply.len,
                        hop_limit: reply.hop_limit,
                    }
                    .emit(&mut ipv4, &ChecksumCapabilities::default());
                    let len = ETHERNET_HEADER_LEN + IPV4_HEADER_LEN + reply.len;
                    client.recv(&eth.as_ref()[..len], &ChecksumState::IPV4_ONLY);
                    self.stats.rx_packets.increment();
                    self.last_activity = Instant::now();
                }
                Poll::Ready(Err(err)) => {
                    tracelimit::error_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        guest_ip = %dst_addr.ip(),
                        "icmp recv error"
                    );
                    break false;
                }
                Poll::Pending => break true,
            }
        }
    }

    fn recv_from(socket: &Socket, buffer: &mut [u8]) -> std::io::Result<(usize, SockAddr)> {
        // SAFETY: The underlying socket `recv` implementation promises
        //   not to write uninitialized bytes into the buffer.
        //   We use ptr::slice_from_raw_parts_mut to create a proper MaybeUninit slice.
        let buf = unsafe {
            std::slice::from_raw_parts_mut(
                buffer.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                buffer.len(),
            )
        };
        let (read_count, addr) = socket.recv_from(buf)?;
        Ok((read_count, addr))
    }

    fn send_to(&mut self, dest: Ipv4Addr, buffer: &[u8], hop_limit: u8) -> std::io::Result<()> {
        let socket = self.socket.as_ref().unwrap().get();
        let dest = SocketAddr::new(IpAddr::V4(dest), 0);
        socket.set_ttl_v4(hop_limit as u32)?;
        socket.send_to(buffer, &dest.into())?;
        Ok(())
    }
}

impl<T: Client> Access<'_, T> {
    pub(crate) fn refresh_icmp_driver(&mut self) {
        self.inner.icmp.connections.retain(|guest_addr, conn| {
            let socket = conn.socket.take().unwrap().into_inner();
            match PolledSocket::new(self.client.driver(), socket) {
                Ok(socket) => {
                    conn.socket = Some(socket);
                    true
                }
                Err(err) => {
                    tracelimit::warn_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        guest_ip = %guest_addr.ip(),
                        "failed to update driver for icmp connection"
                    );
                    false
                }
            }
        });
    }

    pub(crate) fn poll_icmp(&mut self, cx: &mut Context<'_>) {
        let timeout = self.inner.icmp.timeout;
        let now = Instant::now();
        self.inner.icmp.connections.retain(|dst_addr, conn| {
            if now.duration_since(conn.last_activity) > timeout {
                tracing::debug!(
                    guest = %dst_addr,
                    "ICMP connection timed out"
                );
                return false;
            }

            conn.poll_conn(cx, dst_addr, &mut self.inner.state, self.client)
        });
    }

    /// Handle an ICMP echo request destined for the gateway IP by
    /// generating a reply directly, without involving a host socket.
    /// This allows the guest to measure VMM round-trip latency via ping.
    fn handle_icmp_gateway_echo(
        &mut self,
        frame: &EthernetRepr,
        addresses: &Ipv4Addresses,
        payload: &[u8],
    ) -> Result<(), DropReason> {
        if payload.len() < ICMPV4_HEADER_LEN {
            return Err(DropReason::MalformedPacket);
        }

        let icmp_packet = Icmpv4Packet::new_unchecked(payload);
        if icmp_packet.msg_type() != Icmpv4Message::EchoRequest {
            // The gateway only responds to echo requests; silently drop
            // anything else (e.g. timestamp, info request).
            return Ok(());
        }

        let icmp_len = payload.len();
        let ipv4_total_len = IPV4_HEADER_LEN + icmp_len;
        let eth_total_len = ETHERNET_HEADER_LEN + ipv4_total_len;
        if eth_total_len > MIN_MTU {
            return Err(DropReason::MalformedPacket);
        }

        let mut buffer = [0u8; MIN_MTU];

        // Ethernet header
        let resp_eth = EthernetRepr {
            src_addr: self.inner.state.params.gateway_mac,
            dst_addr: frame.src_addr,
            ethertype: EthernetProtocol::Ipv4,
        };
        let mut eth = EthernetFrame::new_unchecked(&mut buffer[..]);
        resp_eth.emit(&mut eth);

        // IPv4 header
        let resp_ipv4 = Ipv4Repr {
            src_addr: self.inner.state.params.gateway_ip,
            dst_addr: addresses.src_addr,
            next_header: IpProtocol::Icmp,
            payload_len: icmp_len,
            hop_limit: 64,
        };
        let mut ipv4 = Ipv4Packet::new_unchecked(eth.payload_mut());
        resp_ipv4.emit(&mut ipv4, &ChecksumCapabilities::default());

        // ICMP echo reply — copy the request payload and change the type.
        let icmp_buf = &mut ipv4.payload_mut()[..icmp_len];
        icmp_buf.copy_from_slice(payload);
        let mut icmp_reply = Icmpv4Packet::new_unchecked(icmp_buf);
        icmp_reply.set_msg_type(Icmpv4Message::EchoReply);
        icmp_reply.fill_checksum();

        self.client
            .recv(&buffer[..eth_total_len], &ChecksumState::IPV4_ONLY);
        Ok(())
    }

    pub(crate) fn handle_icmp(
        &mut self,
        frame: &EthernetRepr,
        addresses: &Ipv4Addresses,
        payload: &[u8],
        _checksum: &ChecksumState,
        hop_limit: u8,
    ) -> Result<(), DropReason> {
        // Respond to pings aimed at the gateway directly, giving the guest
        // a way to measure VMM round-trip time without host socket overhead.
        if addresses.dst_addr == self.inner.state.params.gateway_ip {
            return self.handle_icmp_gateway_echo(frame, addresses, payload);
        }

        // Only echo requests leave: the relay forwards no other message that
        // the guest could forge, nor an echo request that it cannot read.
        let request = Icmpv4Packet::new_checked(payload)?;
        if request.msg_type() != Icmpv4Message::EchoRequest || request.msg_code() != 0 {
            return Err(DropReason::UnsupportedIcmpv4(request.msg_type()));
        }
        let guest_addr = SocketAddrV4::new(addresses.src_addr, 0);

        self.inner.icmp.check_flow_limit(&guest_addr)?;

        let entry = self.inner.icmp.connections.entry(guest_addr);
        let conn = match entry {
            hash_map::Entry::Occupied(conn) => conn.into_mut(),
            hash_map::Entry::Vacant(e) => {
                // Linux restricts opening of 'RAW' sockets without 'CAP_NET_RAW'
                // permission. But, it allows user mode DGRAM + ICMP_PROTO sockets
                // with the 'net.ipv4.ping_group_range' configuration, which is more
                // permissive.
                let socket_type = match Delivery::HOST {
                    Delivery::Raw => Type::RAW,
                    Delivery::Packet | Delivery::Message => Type::DGRAM,
                };
                let mut socket =
                    match Socket::new(Domain::IPV4, socket_type, Some(Protocol::ICMPV4)) {
                        Err(e) => {
                            tracelimit::error_ratelimited!(
                                error = &e as &dyn std::error::Error,
                                guest_ip = %addresses.src_addr,
                                dst_ip = %addresses.dst_addr,
                                "icmp socket creation failed",
                            );
                            return Err(DropReason::Io(e));
                        }
                        Ok(s) => s,
                    };
                Self::bind(&mut socket, Ipv4Addr::UNSPECIFIED).map_err(DropReason::Io)?;
                let socket =
                    PolledSocket::new(self.client.driver(), socket).map_err(DropReason::Io)?;
                let conn = IcmpConnection {
                    socket: Some(socket),
                    guest_mac: frame.src_addr,
                    echoes: PendingEchoes::random(),
                    stats: Default::default(),
                    last_activity: Instant::now(),
                };
                e.insert(conn)
            }
        };

        // The request leaves under the numbers that its reply returns.
        let mut message = payload.to_vec();
        let host_seq = conn.echoes.relay_request(
            Delivery::HOST,
            &mut message,
            addresses.dst_addr,
            Instant::now(),
        );
        match conn.send_to(addresses.dst_addr, &message, hop_limit) {
            Ok(_) => {
                conn.stats.tx_packets.increment();
                conn.last_activity = Instant::now();
                Ok(())
            }
            Err(err) => {
                conn.echoes.forget(host_seq);
                if err.kind() == ErrorKind::WouldBlock {
                    conn.stats.tx_dropped.increment();
                    Err(DropReason::SendBufferFull)
                } else {
                    conn.stats.tx_errors.increment();
                    Err(DropReason::Io(err))
                }
            }
        }
    }

    fn bind<A: Into<Ipv4Addr>>(socket: &mut Socket, addr: A) -> std::io::Result<()> {
        let addr = SocketAddr::new(IpAddr::V4(addr.into()), 0);
        socket.bind(&addr.into())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    mod echoes;
    mod limits;

    use self::echoes::GUEST_IDENT;
    use self::echoes::delivered;
    use self::echoes::flow;
    use self::echoes::icmp_message;
    use self::echoes::wire_ident;
    use super::*;
    use crate::Consomme;
    use crate::ConsommeParams;
    use crate::tests::DriverThread;
    use crate::tests::TestClient;
    use crate::tests::WakeChannel;
    use std::net::UdpSocket;
    use test_with_tracing::test;

    #[test]
    fn icmp_connection_survives_driver_refresh() {
        let (old_thread, old_driver) = DriverThread::new();
        let (new_thread, new_driver) = DriverThread::new();
        let params = ConsommeParams::new().unwrap();
        let guest_addr = SocketAddrV4::new(params.client_ip, 0);
        let guest_mac = params.client_mac;
        let mut consomme = Consomme::new(params);
        let mut client = TestClient::new(old_driver);

        // Feed what a host ICMP socket delivers through UDP so this test needs no
        // ICMP-socket privileges. The replies answer two requests of the flow.
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let host_addr = socket.local_addr().unwrap();
        let socket = PolledSocket::new(&client.driver, Socket::from(socket)).unwrap();
        let mut echoes = flow();
        let now = Instant::now();
        let old_seq = echoes.insert(GUEST_IDENT, 1, Ipv4Addr::LOCALHOST, now);
        let new_seq = echoes.insert(GUEST_IDENT, 2, Ipv4Addr::LOCALHOST, now);
        consomme.icmp.connections.insert(
            guest_addr,
            IcmpConnection {
                socket: Some(socket),
                guest_mac,
                echoes,
                stats: Default::default(),
                last_activity: Instant::now(),
            },
        );
        let old_wake = WakeChannel::new();
        old_wake.poll(&mut consomme, &mut client);
        assert!(client.received_packets.is_empty());

        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let send = |seq: u16, data: &[u8]| {
            let message = icmp_message(
                Icmpv4Message::EchoReply,
                wire_ident(Delivery::HOST),
                seq,
                data,
            );
            let delivered = delivered(Delivery::HOST, Ipv4Addr::LOCALHOST, &message);
            sender.send_to(&delivered, host_addr).unwrap();
        };
        send(old_seq, b"old!");
        client.driver = new_driver;
        old_thread.join();
        consomme.access(&mut client).refresh_driver();
        consomme.access(&mut client).refresh_driver();
        let wake = WakeChannel::new();
        wake.poll(&mut consomme, &mut client);
        if client.received_packets.is_empty() {
            wake.wait_for_packet(&mut consomme, &mut client, |_| true);
        }
        let frame = EthernetFrame::new_checked(&client.received_packets[0]).unwrap();
        let reply = Ipv4Packet::new_checked(frame.payload()).unwrap();
        assert_eq!(frame.dst_addr(), guest_mac);
        assert_eq!(reply.dst_addr(), *guest_addr.ip());
        assert_eq!(&reply.payload()[ICMPV4_HEADER_LEN..], b"old!");

        client.received_packets.clear();
        wake.poll(&mut consomme, &mut client);
        send(new_seq, b"new!");
        wake.wait_for_packet(&mut consomme, &mut client, |_| true);
        let frame = EthernetFrame::new_checked(&client.received_packets[0]).unwrap();
        let reply = Ipv4Packet::new_checked(frame.payload()).unwrap();
        assert_eq!(&reply.payload()[ICMPV4_HEADER_LEN..], b"new!");
        drop((consomme, client));
        new_thread.join();
    }
}
