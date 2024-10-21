// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::dhcp::DHCP_SERVER;
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
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::EthernetAddress;
use smoltcp::wire::EthernetFrame;
use smoltcp::wire::EthernetProtocol;
use smoltcp::wire::EthernetRepr;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Packet;
use smoltcp::wire::Ipv4Repr;
use smoltcp::wire::UdpPacket;
use smoltcp::wire::UdpRepr;
use smoltcp::wire::ETHERNET_HEADER_LEN;
use smoltcp::wire::IPV4_HEADER_LEN;
use smoltcp::wire::UDP_HEADER_LEN;
use std::collections::hash_map;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::UdpSocket;
use std::task::Context;
use std::task::Poll;

pub(crate) struct Udp {
    connections: HashMap<SocketAddress, UdpConnection>,
}

impl Udp {
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
        }
    }
}

impl Inspect for Udp {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        for (addr, conn) in &self.connections {
            resp.field(&format!("{}:{}", addr.ip, addr.port), conn);
        }
    }
}

struct UdpConnection {
    socket: PolledSocket<UdpSocket>,
    guest_mac: EthernetAddress,
}

impl Inspect for UdpConnection {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond();
    }
}

impl UdpConnection {
    fn poll_conn(
        &mut self,
        cx: &mut Context<'_>,
        dst_addr: &SocketAddress,
        state: &mut ConsommeState,
        client: &mut impl Client,
    ) {
        let mut eth = EthernetFrame::new_unchecked(&mut state.buffer);
        // Receive UDP packets while there are receive buffers available. This
        // means we won't drop UDP packets at this level--instead, we only drop
        // UDP packets if the kernel socket's receive buffer fills up. If this
        // results in latency problems, then we could try sizing this buffer
        // more carefully.
        while client.rx_mtu() > 0 {
            match self
                .socket
                .poll_io(cx, InterestSlot::Read, PollEvents::IN, |socket| {
                    socket
                        .get()
                        .recv_from(&mut eth.payload_mut()[IPV4_HEADER_LEN + UDP_HEADER_LEN..])
                }) {
                Poll::Ready(Ok((n, src_addr))) => {
                    let src_ip = if let IpAddr::V4(ip) = src_addr.ip() {
                        ip
                    } else {
                        unreachable!()
                    };
                    eth.set_ethertype(EthernetProtocol::Ipv4);
                    eth.set_src_addr(state.gateway_mac);
                    eth.set_dst_addr(self.guest_mac);
                    let mut ipv4 = Ipv4Packet::new_unchecked(eth.payload_mut());
                    Ipv4Repr {
                        src_addr: src_ip.into(),
                        dst_addr: dst_addr.ip,
                        protocol: IpProtocol::Udp,
                        payload_len: UDP_HEADER_LEN + n,
                        hop_limit: 64,
                    }
                    .emit(&mut ipv4, &ChecksumCapabilities::default());
                    let mut udp = UdpPacket::new_unchecked(ipv4.payload_mut());
                    udp.set_src_port(src_addr.port());
                    udp.set_dst_port(dst_addr.port);
                    udp.set_len((UDP_HEADER_LEN + n) as u16);
                    udp.fill_checksum(&src_ip.into(), &dst_addr.ip.into());
                    let len = ETHERNET_HEADER_LEN + ipv4.total_len() as usize;
                    client.recv(&eth.as_ref()[..len], &ChecksumState::UDP4);
                }
                Poll::Ready(Err(err)) => {
                    tracing::error!(error = &err as &dyn std::error::Error, "recv error");
                    break;
                }
                Poll::Pending => break,
            }
        }
    }
}

impl<T: Client> Access<'_, T> {
    pub(crate) fn poll_udp(&mut self, cx: &mut Context<'_>) {
        for (dst_addr, conn) in &mut self.inner.udp.connections {
            conn.poll_conn(cx, dst_addr, &mut self.inner.state, self.client);
        }
    }

    pub(crate) fn handle_udp(
        &mut self,
        frame: &EthernetRepr,
        addresses: &Ipv4Addresses,
        payload: &[u8],
        checksum: &ChecksumState,
    ) -> Result<(), DropReason> {
        let udp_packet = UdpPacket::new_checked(payload)?;
        let udp = UdpRepr::parse(
            &udp_packet,
            &addresses.src_addr.into(),
            &addresses.dst_addr.into(),
            &checksum.caps(),
        )?;

        if addresses.dst_addr == self.inner.state.gateway_ip || addresses.dst_addr.is_broadcast() {
            if self.handle_gateway_udp(&udp_packet)? {
                return Ok(());
            }
        }

        let guest_addr = SocketAddress {
            ip: addresses.src_addr,
            port: udp.src_port,
        };

        let conn = self.get_or_insert(guest_addr, None, Some(frame.src_addr))?;
        match conn.socket.get().send_to(
            udp_packet.payload(),
            (Ipv4Addr::from(addresses.dst_addr), udp.dst_port),
        ) {
            Ok(_) => Ok(()),
            Err(err) if err.kind() == ErrorKind::WouldBlock => Err(DropReason::SendBufferFull),
            Err(err) => Err(DropReason::Io(err)),
        }
    }

    fn get_or_insert(
        &mut self,
        guest_addr: SocketAddress,
        host_addr: Option<Ipv4Addr>,
        guest_mac: Option<EthernetAddress>,
    ) -> Result<&mut UdpConnection, DropReason> {
        let entry = self.inner.udp.connections.entry(guest_addr);
        match entry {
            hash_map::Entry::Occupied(conn) => Ok(conn.into_mut()),
            hash_map::Entry::Vacant(e) => {
                let socket = UdpSocket::bind((host_addr.unwrap_or(Ipv4Addr::UNSPECIFIED), 0))
                    .map_err(DropReason::Io)?;
                let socket =
                    PolledSocket::new(self.client.driver(), socket).map_err(DropReason::Io)?;
                let conn = UdpConnection {
                    socket,
                    guest_mac: guest_mac.unwrap_or(self.inner.state.client_mac),
                };
                Ok(e.insert(conn))
            }
        }
    }

    fn handle_gateway_udp(&mut self, udp: &UdpPacket<&[u8]>) -> Result<bool, DropReason> {
        let payload = udp.payload();
        match udp.dst_port() {
            DHCP_SERVER => {
                self.handle_dhcp(payload)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    pub(crate) fn bind_udp_port(
        &mut self,
        ip_addr: Option<Ipv4Addr>,
        port: u16,
    ) -> Result<(), DropReason> {
        let guest_addr = SocketAddress {
            ip: ip_addr.unwrap_or(Ipv4Addr::UNSPECIFIED).into(),
            port,
        };
        let _ = self.get_or_insert(guest_addr, ip_addr, None)?;
        Ok(())
    }

    pub(crate) fn unbind_udp_port(&mut self, port: u16) -> Result<(), DropReason> {
        let guest_addr = SocketAddress {
            ip: Ipv4Addr::UNSPECIFIED.into(),
            port,
        };
        match self.inner.udp.connections.remove(&guest_addr) {
            Some(_) => Ok(()),
            None => Err(DropReason::PortNotBound),
        }
    }
}
