// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Access;
use super::Client;
use super::DropReason;
use super::dhcp::DHCP_SERVER;
use crate::ChecksumState;
use crate::ConsommeState;
use crate::IpAddresses;
use crate::IpSocketAddress;
use dhcproto::v6::SERVER_PORT as DHCPV6_SERVER_PORT;
use inspect::Inspect;
use inspect::InspectMut;
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
use smoltcp::wire::IPV6_HEADER_LEN;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Packet;
use smoltcp::wire::Ipv4Repr;
use smoltcp::wire::Ipv6Address;
use smoltcp::wire::Ipv6Packet;
use smoltcp::wire::Ipv6Repr;
use smoltcp::wire::UDP_HEADER_LEN;
use smoltcp::wire::UdpPacket;
use smoltcp::wire::UdpRepr;
use std::collections::HashMap;
use std::collections::hash_map;
use std::io::ErrorKind;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::UdpSocket;
use std::task::Context;
use std::task::Poll;

pub(crate) struct Udp {
    connections: HashMap<IpSocketAddress, UdpConnection>,
}

impl Udp {
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
        }
    }
}

impl InspectMut for Udp {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        for (addr, conn) in &mut self.connections {
            let key = match addr {
                IpSocketAddress::V4 { ip, port } => format!("{}:{}", ip, port),
                IpSocketAddress::V6 { ip, port } => format!("[{}]:{}", ip, port),
            };
            resp.field_mut(&key, conn);
        }
    }
}

#[derive(InspectMut)]
struct UdpConnection {
    #[inspect(skip)]
    socket: Option<PolledSocket<UdpSocket>>,
    #[inspect(display)]
    guest_mac: EthernetAddress,
    stats: Stats,
    #[inspect(mut)]
    recycle: bool,
}

#[derive(Inspect, Default)]
struct Stats {
    tx_packets: Counter,
    tx_dropped: Counter,
    tx_errors: Counter,
    rx_packets: Counter,
}

impl UdpConnection {
    fn poll_conn(
        &mut self,
        cx: &mut Context<'_>,
        dst_addr: &IpSocketAddress,
        state: &mut ConsommeState,
        client: &mut impl Client,
    ) -> bool {
        if self.recycle {
            return false;
        }

        let mut eth = EthernetFrame::new_unchecked(&mut state.buffer);
        loop {
            // Receive UDP packets while there are receive buffers available. This
            // means we won't drop UDP packets at this level--instead, we only drop
            // UDP packets if the kernel socket's receive buffer fills up. If this
            // results in latency problems, then we could try sizing this buffer
            // more carefully.
            if client.rx_mtu() == 0 {
                break true;
            }

            let header_offset = match dst_addr {
                IpSocketAddress::V4 { .. } => IPV4_HEADER_LEN + UDP_HEADER_LEN,
                IpSocketAddress::V6 { .. } => IPV6_HEADER_LEN + UDP_HEADER_LEN,
            };

            match self.socket.as_mut().unwrap().poll_io(
                cx,
                InterestSlot::Read,
                PollEvents::IN,
                |socket| {
                    socket
                        .get()
                        .recv_from(&mut eth.payload_mut()[header_offset..])
                },
            ) {
                Poll::Ready(Ok((n, src_addr))) => {
                    // Set common Ethernet header fields
                    eth.set_src_addr(state.params.gateway_mac);
                    eth.set_dst_addr(self.guest_mac);

                    // Build IP and UDP headers based on address version
                    let (packet_len, checksum_state) = match (src_addr.ip(), dst_addr) {
                        (
                            IpAddr::V4(src_ip),
                            IpSocketAddress::V4 {
                                ip: dst_ip,
                                port: dst_port,
                            },
                        ) => {
                            eth.set_ethertype(EthernetProtocol::Ipv4);
                            let mut ipv4 = Ipv4Packet::new_unchecked(eth.payload_mut());
                            Ipv4Repr {
                                src_addr: src_ip.into(),
                                dst_addr: *dst_ip,
                                protocol: IpProtocol::Udp,
                                payload_len: UDP_HEADER_LEN + n,
                                hop_limit: 64,
                            }
                            .emit(&mut ipv4, &ChecksumCapabilities::default());

                            // Build UDP header
                            let mut udp = UdpPacket::new_unchecked(ipv4.payload_mut());
                            udp.set_src_port(src_addr.port());
                            udp.set_dst_port(*dst_port);
                            udp.set_len((UDP_HEADER_LEN + n) as u16);
                            udp.fill_checksum(&src_ip.into(), &(*dst_ip).into());

                            (
                                ETHERNET_HEADER_LEN + ipv4.total_len() as usize,
                                ChecksumState::UDP4,
                            )
                        }
                        (
                            IpAddr::V6(src_ip),
                            IpSocketAddress::V6 {
                                ip: dst_ip,
                                port: dst_port,
                            },
                        ) => {
                            eth.set_ethertype(EthernetProtocol::Ipv6);
                            let mut ipv6 = Ipv6Packet::new_unchecked(eth.payload_mut());
                            Ipv6Repr {
                                src_addr: src_ip.into(),
                                dst_addr: *dst_ip,
                                next_header: IpProtocol::Udp,
                                payload_len: UDP_HEADER_LEN + n,
                                hop_limit: 64,
                            }
                            .emit(&mut ipv6);

                            // Build UDP header
                            let mut udp = UdpPacket::new_unchecked(ipv6.payload_mut());
                            udp.set_src_port(src_addr.port());
                            udp.set_dst_port(*dst_port);
                            udp.set_len((UDP_HEADER_LEN + n) as u16);
                            udp.fill_checksum(&src_ip.into(), &(*dst_ip).into());

                            (
                                ETHERNET_HEADER_LEN + IPV6_HEADER_LEN + UDP_HEADER_LEN + n,
                                ChecksumState::NONE,
                            )
                        }
                        _ => {
                            tracing::error!(
                                "IP version mismatch between socket and destination address"
                            );
                            break false;
                        }
                    };

                    // Send packet to client
                    client.recv(&eth.as_ref()[..packet_len], &checksum_state);
                    self.stats.rx_packets.increment();
                }
                Poll::Ready(Err(err)) => {
                    tracing::error!(error = &err as &dyn std::error::Error, "recv error");
                    break false;
                }
                Poll::Pending => break true,
            }
        }
    }
}

impl<T: Client> Access<'_, T> {
    pub(crate) fn poll_udp(&mut self, cx: &mut Context<'_>) {
        self.inner.udp.connections.retain(|dst_addr, conn| {
            conn.poll_conn(cx, dst_addr, &mut self.inner.state, self.client)
        });
    }

    pub(crate) fn refresh_udp_driver(&mut self) {
        self.inner.udp.connections.retain(|_, conn| {
            let socket = conn.socket.take().unwrap().into_inner();
            match PolledSocket::new(self.client.driver(), socket) {
                Ok(socket) => {
                    conn.socket = Some(socket);
                    true
                }
                Err(err) => {
                    tracing::warn!(
                        error = &err as &dyn std::error::Error,
                        "failed to update driver for udp connection"
                    );
                    false
                }
            }
        });
    }

    pub(crate) fn handle_udp(
        &mut self,
        frame: &EthernetRepr,
        addresses: &IpAddresses,
        payload: &[u8],
        checksum: &ChecksumState,
    ) -> Result<(), DropReason> {
        let udp_packet = UdpPacket::new_checked(payload)?;

        // Parse UDP header and check gateway handling
        let (guest_addr, dst_sock_addr) = match addresses {
            IpAddresses::V4(addrs) => {
                let udp = UdpRepr::parse(
                    &udp_packet,
                    &addrs.src_addr.into(),
                    &addrs.dst_addr.into(),
                    &checksum.caps(),
                )?;

                // Check for gateway-destined packets
                if addrs.dst_addr == self.inner.state.params.gateway_ip
                    || addrs.dst_addr.is_broadcast()
                {
                    if self.handle_gateway_udp(&udp_packet)? {
                        return Ok(());
                    }
                }

                let guest_addr = IpSocketAddress::V4 {
                    ip: addrs.src_addr,
                    port: udp.src_port,
                };

                let dst_sock_addr = std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
                    addrs.dst_addr.into(),
                    udp.dst_port,
                ));

                (guest_addr, dst_sock_addr)
            }
            IpAddresses::V6(addrs) => {
                let udp = UdpRepr::parse(
                    &udp_packet,
                    &addrs.src_addr.into(),
                    &addrs.dst_addr.into(),
                    &checksum.caps(),
                )?;

                // Check for gateway-destined packets (IPv6 uses multicast instead of broadcast)
                if addrs.dst_addr == self.inner.state.params.gateway_link_local_ipv6
                    || addrs.dst_addr.is_multicast()
                {
                    if self.handle_gateway_udp_v6(&udp_packet, Some(addrs.src_addr))? {
                        return Ok(());
                    }
                }
                

                let guest_addr = IpSocketAddress::V6 {
                    ip: addrs.src_addr,
                    port: udp.src_port,
                };

                let dst_sock_addr = std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
                    addrs.dst_addr.into(),
                    udp.dst_port,
                    0,
                    0,
                ));

                (guest_addr, dst_sock_addr)
            }
        };

        let conn = self.get_or_insert(guest_addr, Some(frame.src_addr))?;
        match conn
            .socket
            .as_mut()
            .unwrap()
            .get()
            .send_to(udp_packet.payload(), dst_sock_addr)
        {
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

    fn get_or_insert(
        &mut self,
        guest_addr: IpSocketAddress,
        guest_mac: Option<EthernetAddress>,
    ) -> Result<&mut UdpConnection, DropReason> {
        let entry = self.inner.udp.connections.entry(guest_addr);
        match entry {
            hash_map::Entry::Occupied(conn) => Ok(conn.into_mut()),
            hash_map::Entry::Vacant(e) => {
                let bind_addr: std::net::SocketAddr = match guest_addr {
                    IpSocketAddress::V4 { .. } => std::net::SocketAddr::V4(
                        std::net::SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0),
                    ),
                    IpSocketAddress::V6 { .. } => std::net::SocketAddr::V6(
                        std::net::SocketAddrV6::new(std::net::Ipv6Addr::UNSPECIFIED, 0, 0, 0),
                    ),
                };

                let socket = UdpSocket::bind(bind_addr).map_err(DropReason::Io)?;
                let socket =
                    PolledSocket::new(self.client.driver(), socket).map_err(DropReason::Io)?;
                let conn = UdpConnection {
                    socket: Some(socket),
                    guest_mac: guest_mac.unwrap_or(self.inner.state.params.client_mac),
                    stats: Default::default(),
                    recycle: false,
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

    fn handle_gateway_udp_v6(&mut self, udp: &UdpPacket<&[u8]>, client_ip: Option<Ipv6Address>) -> Result<bool, DropReason> {
        let payload = udp.payload();
        match udp.dst_port() {
            DHCPV6_SERVER_PORT => {
                self.handle_dhcpv6(payload, client_ip)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Binds to the specified host IP and port for forwarding inbound UDP
    /// packets to the guest.
    pub fn bind_udp_port(&mut self, ip_addr: Option<IpAddr>, port: u16) -> Result<(), DropReason> {
        let guest_addr = match ip_addr {
            Some(IpAddr::V4(ip)) => IpSocketAddress::V4 {
                ip: ip.into(),
                port,
            },
            Some(IpAddr::V6(ip)) => IpSocketAddress::V6 {
                ip: ip.into(),
                port,
            },
            None => IpSocketAddress::V4 {
                ip: Ipv4Addr::UNSPECIFIED.into(),
                port,
            },
        };
        let _ = self.get_or_insert(guest_addr, None)?;
        Ok(())
    }

    /// Unbinds from the specified host port for both IPv4 and IPv6.
    pub fn unbind_udp_port(&mut self, port: u16) -> Result<(), DropReason> {
        // Try to remove both IPv4 and IPv6 bindings
        let v4_addr = IpSocketAddress::V4 {
            ip: Ipv4Addr::UNSPECIFIED.into(),
            port,
        };
        let v6_addr = IpSocketAddress::V6 {
            ip: std::net::Ipv6Addr::UNSPECIFIED.into(),
            port,
        };

        let v4_removed = self.inner.udp.connections.remove(&v4_addr).is_some();
        let v6_removed = self.inner.udp.connections.remove(&v6_addr).is_some();

        if v4_removed || v6_removed {
            Ok(())
        } else {
            Err(DropReason::PortNotBound)
        }
    }
}
