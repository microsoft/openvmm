// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: needed to cast the socket buffer to `MaybeUninit`.
#![expect(unsafe_code)]

use super::Access;
use super::Client;
use super::ConsommeState;
use super::DropReason;
use crate::ChecksumState;
use crate::Ipv4Addresses;
use crate::MIN_MTU;

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

const ICMPV4_HEADER_LEN: usize = 8;

pub(crate) struct Icmp {
    connections: HashMap<SocketAddrV4, IcmpConnection>,
}

impl Icmp {
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
        }
    }
}

impl Inspect for Icmp {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        for (addr, conn) in &self.connections {
            resp.field(&format!("{}:{}", addr.ip(), addr.port()), conn);
        }
    }
}

#[derive(Inspect)]
struct IcmpConnection {
    #[inspect(skip)]
    socket: PolledSocket<Socket>,
    #[inspect(display)]
    guest_mac: EthernetAddress,
    stats: Stats,
}

#[derive(Inspect, Default)]
struct Stats {
    tx_packets: Counter,
    tx_dropped: Counter,
    tx_errors: Counter,
    rx_packets: Counter,
}

impl IcmpConnection {
    fn poll_conn(
        &mut self,
        cx: &mut Context<'_>,
        dst_addr: &SocketAddrV4,
        state: &mut ConsommeState,
        client: &mut impl Client,
    ) {
        let mut eth = EthernetFrame::new_unchecked(&mut state.buffer);
        loop {
            match self
                .socket
                .poll_io(cx, InterestSlot::Read, PollEvents::IN, |socket| {
                    Self::recv_from(socket.get(), &mut eth.payload_mut()[..])
                }) {
                Poll::Ready(Ok((n, _))) => {
                    if n < IPV4_HEADER_LEN + ICMPV4_HEADER_LEN {
                        tracelimit::warn_ratelimited!(
                            guest_ip = %dst_addr.ip(),
                            "dropping malformed ICMP incoming packet",
                        );
                        continue;
                    }

                    // What is received is a raw IPV4 packet. Add the Ethernet frame and
                    // set the destination address in the IP header.
                    eth.set_ethertype(EthernetProtocol::Ipv4);
                    eth.set_src_addr(state.params.gateway_mac);
                    eth.set_dst_addr(self.guest_mac);
                    let mut ipv4 = Ipv4Packet::new_unchecked(eth.payload_mut());
                    ipv4.set_dst_addr(*dst_addr.ip());
                    ipv4.fill_checksum();
                    let len = ETHERNET_HEADER_LEN + n;
                    client.recv(&eth.as_ref()[..len], &ChecksumState::IPV4_ONLY);
                    self.stats.rx_packets.increment();
                }
                Poll::Ready(Err(err)) => {
                    tracelimit::error_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        guest_ip = %dst_addr.ip(),
                        "icmp recv error"
                    );
                    break;
                }
                Poll::Pending => break,
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
        let socket = self.socket.get();
        let dest = SocketAddr::new(IpAddr::V4(dest), 0);
        socket.set_ttl_v4(hop_limit as u32)?;
        socket.send_to(buffer, &dest.into())?;
        Ok(())
    }
}

impl<T: Client> Access<'_, T> {
    pub(crate) fn poll_icmp(&mut self, cx: &mut Context<'_>) {
        for (dst_addr, conn) in &mut self.inner.icmp.connections {
            conn.poll_conn(cx, dst_addr, &mut self.inner.state, self.client);
        }
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

        let icmp_packet = Icmpv4Packet::new_unchecked(payload);
        let guest_addr = SocketAddrV4::new(addresses.src_addr, 0);

        let entry = self.inner.icmp.connections.entry(guest_addr);
        let conn = match entry {
            hash_map::Entry::Occupied(conn) => conn.into_mut(),
            hash_map::Entry::Vacant(e) => {
                // Linux restricts opening of 'RAW' sockets without 'CAP_NET_RAW'
                // permission. But, it allows user mode DGRAM + ICMP_PROTO sockets
                // with the 'net.ip.ping_group_range' configuration, which is more
                // permissive.
                let socket_type = if cfg!(windows) {
                    Type::RAW
                } else {
                    Type::DGRAM
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
                    socket,
                    guest_mac: frame.src_addr,
                    stats: Default::default(),
                };
                e.insert(conn)
            }
        };

        let send_buffer = icmp_packet.into_inner();
        match conn.send_to(addresses.dst_addr, send_buffer, hop_limit) {
            Ok(_) => {
                conn.stats.tx_packets.increment();
                Ok(())
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                conn.stats.tx_dropped.increment();
                Err(DropReason::SendBufferFull)
            }
            Err(err) => {
                conn.stats.tx_errors.increment();
                Err(DropReason::Io(err))
            }
        }
    }

    fn bind<A: Into<Ipv4Addr>>(socket: &mut Socket, addr: A) -> std::io::Result<()> {
        let addr = SocketAddr::new(IpAddr::V4(addr.into()), 0);
        socket.bind(&addr.into())?;
        Ok(())
    }
}
