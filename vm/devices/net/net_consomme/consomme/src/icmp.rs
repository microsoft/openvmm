// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Copyright (C) Microsoft Corporation. All rights reserved.

// UNSAFETY: needed to cast the socket buffer to `MaybeUninit`.
#![allow(unsafe_code)]
#![allow(clippy::undocumented_unsafe_blocks)]

use super::Access;
use super::Client;
use super::ConsommeState;
use super::DropReason;
use super::SocketAddress;
use crate::ChecksumState;
use crate::Ipv4Addresses;

use inspect::Inspect;
use pal_async::interest::InterestSlot;
use pal_async::interest::PollEvents;
use pal_async::socket::PolledSocket;
use smoltcp::wire::EthernetAddress;
use smoltcp::wire::EthernetFrame;
use smoltcp::wire::EthernetProtocol;
use smoltcp::wire::EthernetRepr;
use smoltcp::wire::Ipv4Packet;
use smoltcp::wire::ETHERNET_HEADER_LEN;
use smoltcp::wire::IPV4_HEADER_LEN;
use socket2::Domain;
use socket2::Protocol;
use socket2::SockAddr;
use socket2::Socket;
use socket2::Type;
use std::collections::hash_map;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::mem::MaybeUninit;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::task::Context;
use std::task::Poll;

const ICMPV4_HEADER_LEN: usize = 8;

pub(crate) struct Icmp {
    connections: HashMap<SocketAddress, IcmpConnection>,
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
            resp.field(&format!("{}:{}", addr.ip, addr.port), conn);
        }
    }
}

struct IcmpConnection {
    socket: PolledSocket<Socket>,
    guest_mac: EthernetAddress,
}

impl Inspect for IcmpConnection {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond();
    }
}

impl IcmpConnection {
    fn poll_conn(
        &mut self,
        cx: &mut Context<'_>,
        dst_addr: &SocketAddress,
        state: &mut ConsommeState,
        client: &mut impl Client,
    ) {
        match self
            .socket
            .poll_io(cx, InterestSlot::Read, PollEvents::IN, |socket| {
                Self::recv_from(socket.get_mut(), &mut state.buffer[ETHERNET_HEADER_LEN..])
            }) {
            Poll::Ready(Ok((n, _))) => {
                if n < IPV4_HEADER_LEN + ICMPV4_HEADER_LEN {
                    tracing::warn!("dropping malformed ICMP incoming packet");
                    return;
                }
                // What is received is a raw IPV4 packet. Add the Ethernet frame and
                // set the destination address in the IP header.
                let mut eth = EthernetFrame::new_unchecked(&mut state.buffer);
                eth.set_ethertype(EthernetProtocol::Ipv4);
                eth.set_src_addr(state.gateway_mac);
                eth.set_dst_addr(self.guest_mac);
                let mut ipv4 = Ipv4Packet::new_unchecked(eth.payload_mut());
                ipv4.set_dst_addr(dst_addr.ip);
                ipv4.fill_checksum();
                let len = ETHERNET_HEADER_LEN + n;
                client.recv(&state.buffer[..len], &ChecksumState::IPV4_ONLY);
            }
            Poll::Ready(Err(err)) => {
                tracing::error!(error = &err as &dyn std::error::Error, "recv error");
            }
            Poll::Pending => {}
        }
    }

    fn recv_from(socket: &mut Socket, buffer: *mut [u8]) -> std::io::Result<(usize, SockAddr)> {
        // SAFETY: the underlying socket `recv` implementation promises
        // not to write uninitialized bytes into the buffer.
        let buf = unsafe { &mut *(buffer as *mut [MaybeUninit<u8>]) };
        let (read_count, addr) = socket.recv_from(buf)?;
        Ok((read_count, addr))
    }

    fn send_to(&mut self, dest: Ipv4Addr, buffer: &[u8], hop_limit: u8) -> std::io::Result<()> {
        let socket = self.socket.get();
        let dest = SocketAddr::new(IpAddr::V4(dest), 0);
        socket.set_ttl(hop_limit as u32)?;
        socket.send_to(buffer, &(dest.into()))?;
        Ok(())
    }
}

impl<T: Client> Access<'_, T> {
    pub(crate) fn poll_icmp(&mut self, cx: &mut Context<'_>) {
        for (dst_addr, conn) in &mut self.inner.icmp.connections {
            conn.poll_conn(cx, dst_addr, &mut self.inner.state, self.client);
        }
    }

    pub(crate) fn handle_icmp(
        &mut self,
        frame: &EthernetRepr,
        addresses: &Ipv4Addresses,
        payload: &[u8],
        _checksum: &ChecksumState,
        hop_limit: u8,
    ) -> Result<(), DropReason> {
        let icmp_packet = smoltcp::wire::Icmpv4Packet::new_unchecked(payload);
        let guest_addr = SocketAddress {
            ip: addresses.src_addr,
            port: 0,
        };

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
                            tracing::error!("socket creation failed, {}", e);
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
                };
                e.insert(conn)
            }
        };

        let send_buffer = icmp_packet.into_inner();
        let ip4_addr = Ipv4Addr::from(addresses.dst_addr);
        match conn.send_to(ip4_addr, send_buffer, hop_limit) {
            Ok(_) => Ok(()),
            Err(err) if err.kind() == ErrorKind::WouldBlock => Err(DropReason::SendBufferFull),
            Err(err) => Err(err).map_err(DropReason::Io),
        }
    }

    fn bind<A: Into<Ipv4Addr>>(socket: &mut Socket, addr: A) -> std::io::Result<()> {
        let addr = SocketAddr::new(IpAddr::V4(addr.into()), 0);
        socket.bind(&(addr.into()))?;
        Ok(())
    }
}
