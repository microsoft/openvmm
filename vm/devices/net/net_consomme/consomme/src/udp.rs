// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Access;
use super::BindError;
use super::Client;
use super::DropReason;
use super::dhcp::DHCP_SERVER;
use super::dhcpv6::DHCPV6_ALL_AGENTS_MULTICAST;
use super::dhcpv6::DHCPV6_SERVER;
use crate::ChecksumState;
use crate::ConsommeState;
use crate::IpAddresses;
use crate::Ipv4Addresses;
use crate::Ipv6Addresses;
use crate::dns_resolver::DnsFlow;
use crate::dns_resolver::DnsRequest;
use crate::dns_resolver::DnsResponse;
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
use smoltcp::wire::IpAddress;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Packet;
use smoltcp::wire::Ipv4Repr;
use smoltcp::wire::Ipv6Packet;
use smoltcp::wire::Ipv6Repr;
use smoltcp::wire::UDP_HEADER_LEN;
use smoltcp::wire::UdpPacket;
use smoltcp::wire::UdpRepr;
use socket2::Socket;
use std::collections::HashMap;
use std::collections::hash_map;
use std::io::ErrorKind;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::net::SocketAddrV4;
use std::net::SocketAddrV6;
use std::net::UdpSocket;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use std::time::Instant;

use crate::DNS_PORT;

#[cfg(unix)]
use crate::unix as platform;
#[cfg(windows)]
use crate::windows as platform;

pub(crate) struct Udp {
    connections: HashMap<SocketAddr, UdpConnection>,
    listeners: HashMap<u16, UdpListener>,
    timeout: Duration,
}

impl Udp {
    pub fn new(timeout: Duration) -> Self {
        Self {
            connections: HashMap::new(),
            listeners: HashMap::new(),
            timeout,
        }
    }
}

impl InspectMut for Udp {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        for (addr, conn) in &mut self.connections {
            let key = addr.to_string();
            resp.field_mut(&key, conn);
        }
        for (port, listener) in &mut self.listeners {
            resp.field_mut(&format!("listener:{port}"), listener);
        }
    }
}

#[derive(InspectMut)]
struct UdpListener {
    #[inspect(skip)]
    socket: Option<PolledSocket<UdpSocket>>,
    /// The guest port to forward received packets to.
    #[inspect(display)]
    guest_port: u16,
    stats: Stats,
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
    #[inspect(debug)]
    last_activity: Instant,
    gso_size: Option<u16>,
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
        dst_addr: &SocketAddr,
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
                SocketAddr::V4(_) => IPV4_HEADER_LEN + UDP_HEADER_LEN,
                SocketAddr::V6(_) => IPV6_HEADER_LEN + UDP_HEADER_LEN,
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
                    let (packet_len, checksum_state) = match (dst_addr, src_addr.ip()) {
                        (SocketAddr::V4(dst), IpAddr::V4(src_ip)) => {
                            let len = build_udp_packet(
                                &mut eth,
                                src_ip.into(),
                                (*dst.ip()).into(),
                                src_addr.port(),
                                dst.port(),
                                n,
                                state.params.gateway_mac,
                                self.guest_mac,
                            );
                            (len, ChecksumState::UDP4)
                        }
                        (SocketAddr::V6(dst), IpAddr::V6(src_ip)) => {
                            let len = build_udp_packet(
                                &mut eth,
                                src_ip.into(),
                                (*dst.ip()).into(),
                                src_addr.port(),
                                dst.port(),
                                n,
                                state.params.gateway_mac,
                                self.guest_mac,
                            );
                            (len, ChecksumState::NONE)
                        }
                        _ => unreachable!("mismatched address families"),
                    };

                    client.recv(&eth.as_ref()[..packet_len], &checksum_state);
                    self.stats.rx_packets.increment();
                    self.last_activity = Instant::now();
                }
                Poll::Ready(Err(err)) => {
                    tracelimit::error_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        guest = %dst_addr,
                        "udp recv error"
                    );
                    break false;
                }
                Poll::Pending => break true,
            }
        }
    }
}

impl UdpListener {
    fn poll_listener(
        &mut self,
        cx: &mut Context<'_>,
        state: &mut ConsommeState,
        client: &mut impl Client,
    ) {
        let Some(socket) = self.socket.as_mut() else {
            return;
        };
        let mut eth = EthernetFrame::new_unchecked(&mut state.buffer);
        loop {
            if client.rx_mtu() == 0 {
                break;
            }

            // Determine header offset and guest destination from the bound socket's address family.
            let local_addr = match socket.get().local_addr() {
                Ok(addr) => addr,
                Err(_) => break,
            };
            let (header_offset, guest_dst_ip, guest_mac) = match local_addr.ip() {
                IpAddr::V4(_) => (
                    IPV4_HEADER_LEN + UDP_HEADER_LEN,
                    IpAddr::V4(state.params.client_ip),
                    state.params.client_mac,
                ),
                IpAddr::V6(_) => {
                    let Some(client_ipv6) = state.params.client_ip_ipv6 else {
                        break;
                    };
                    (
                        IPV6_HEADER_LEN + UDP_HEADER_LEN,
                        IpAddr::V6(client_ipv6),
                        state.params.client_mac,
                    )
                }
            };

            match socket.poll_io(cx, InterestSlot::Read, PollEvents::IN, |socket| {
                socket
                    .get()
                    .recv_from(&mut eth.payload_mut()[header_offset..])
            }) {
                Poll::Ready(Ok((n, src_addr))) => {
                    let Some((packet_len, checksum_state)) = (match (guest_dst_ip, src_addr.ip()) {
                        (IpAddr::V4(dst_ip), IpAddr::V4(src_ip)) => {
                            let len = build_udp_packet(
                                &mut eth,
                                src_ip.into(),
                                dst_ip.into(),
                                src_addr.port(),
                                self.guest_port,
                                n,
                                state.params.gateway_mac,
                                guest_mac,
                            );
                            Some((len, ChecksumState::UDP4))
                        }
                        (IpAddr::V6(dst_ip), IpAddr::V6(src_ip)) => {
                            let len = build_udp_packet(
                                &mut eth,
                                src_ip.into(),
                                dst_ip.into(),
                                src_addr.port(),
                                self.guest_port,
                                n,
                                state.params.gateway_mac,
                                guest_mac,
                            );
                            Some((len, ChecksumState::NONE))
                        }
                        _ => {
                            tracelimit::warn_ratelimited!(
                                local_addr = %local_addr,
                                src_addr = %src_addr,
                                "udp listener received packet with mismatched address family"
                            );
                            None
                        }
                    }) else {
                        continue;
                    };

                    client.recv(&eth.as_ref()[..packet_len], &checksum_state);
                    self.stats.rx_packets.increment();
                }
                Poll::Ready(Err(err)) => {
                    tracelimit::error_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        local_addr = %local_addr,
                        guest_port = self.guest_port,
                        "udp listener recv error"
                    );
                    break;
                }
                Poll::Pending => break,
            }
        }
    }
}

impl<T: Client> Access<'_, T> {
    pub(crate) fn poll_udp(&mut self, cx: &mut Context<'_>) {
        let timeout = self.inner.udp.timeout;
        let now = Instant::now();

        self.inner.udp.connections.retain(|dst_addr, conn| {
            // Check if connection has timed out
            if now.duration_since(conn.last_activity) > timeout {
                tracing::debug!(
                    guest = %dst_addr,
                    "UDP connection timed out"
                );
                return false;
            }

            conn.poll_conn(cx, dst_addr, &mut self.inner.state, self.client)
        });

        for listener in self.inner.udp.listeners.values_mut() {
            listener.poll_listener(cx, &mut self.inner.state, self.client);
        }

        while let Some(response) =
            self.inner
                .dns
                .as_mut()
                .and_then(|dns| match dns.poll_udp_response(cx) {
                    Poll::Ready(resp) => resp,
                    Poll::Pending => None,
                })
        {
            if let Err(e) = self.send_dns_response(&response) {
                tracelimit::error_ratelimited!(error = ?e, "Failed to send DNS response");
            }
        }
    }

    pub(crate) fn refresh_udp_driver(&mut self) {
        self.inner.udp.connections.retain(|dst_addr, conn| {
            let socket = conn.socket.take().unwrap().into_inner();
            match PolledSocket::new(self.client.driver(), socket) {
                Ok(socket) => {
                    conn.socket = Some(socket);
                    true
                }
                Err(err) => {
                    tracing::warn!(
                        error = &err as &dyn std::error::Error,
                        guest = %dst_addr,
                        "failed to update driver for udp connection"
                    );
                    false
                }
            }
        });
        self.inner.udp.listeners.retain(|port, listener| {
            let socket = listener.socket.take().unwrap().into_inner();
            match PolledSocket::new(self.client.driver(), socket) {
                Ok(socket) => {
                    listener.socket = Some(socket);
                    true
                }
                Err(err) => {
                    tracing::warn!(
                        guest_port = port,
                        error = &err as &dyn std::error::Error,
                        "failed to update driver for udp listener"
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
                    if self.handle_gateway_udp(frame, addrs, &udp_packet)? {
                        return Ok(());
                    }
                }

                let guest_addr = SocketAddr::V4(SocketAddrV4::new(addrs.src_addr, udp.src_port));

                let dst_sock_addr = SocketAddr::V4(SocketAddrV4::new(addrs.dst_addr, udp.dst_port));

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
                    || addrs.dst_addr == DHCPV6_ALL_AGENTS_MULTICAST
                {
                    if self.handle_gateway_udp_v6(frame, addrs, &udp_packet)? {
                        return Ok(());
                    }
                }

                let guest_addr =
                    SocketAddr::V6(SocketAddrV6::new(addrs.src_addr, udp.src_port, 0, 0));

                let dst_sock_addr =
                    SocketAddr::V6(SocketAddrV6::new(addrs.dst_addr, udp.dst_port, 0, 0));

                (guest_addr, dst_sock_addr)
            }
        };

        let conn = self.get_or_insert(guest_addr, None, Some(frame.src_addr))?;
        let socket = conn.socket.as_ref().unwrap().get();
        if conn.gso_size != checksum.gso {
            platform::set_udp_gso_size(socket, checksum.gso.unwrap_or(0))
                .map_err(DropReason::Io)?;
            conn.gso_size = checksum.gso;
        }
        let result = platform::send_to(socket, udp_packet.payload(), &dst_sock_addr, checksum.gso);
        match result {
            Ok(_) => {
                conn.stats.tx_packets.increment();
                conn.last_activity = Instant::now();
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
        guest_addr: SocketAddr,
        host_port: Option<u16>,
        guest_mac: Option<EthernetAddress>,
    ) -> Result<&mut UdpConnection, DropReason> {
        let entry = self.inner.udp.connections.entry(guest_addr);
        match entry {
            hash_map::Entry::Occupied(conn) => Ok(conn.into_mut()),
            hash_map::Entry::Vacant(e) => {
                let bind_addr: SocketAddr = match guest_addr {
                    SocketAddr::V4(_) => SocketAddr::V4(SocketAddrV4::new(
                        Ipv4Addr::UNSPECIFIED,
                        host_port.unwrap_or(0),
                    )),
                    SocketAddr::V6(_) => SocketAddr::V6(SocketAddrV6::new(
                        Ipv6Addr::UNSPECIFIED,
                        host_port.unwrap_or(0),
                        0,
                        0,
                    )),
                };

                let socket = UdpSocket::bind(bind_addr).map_err(DropReason::Io)?;
                let socket =
                    PolledSocket::new(self.client.driver(), socket).map_err(DropReason::Io)?;
                let conn = UdpConnection {
                    socket: Some(socket),
                    guest_mac: guest_mac.unwrap_or(self.inner.state.params.client_mac),
                    stats: Default::default(),
                    recycle: false,
                    last_activity: Instant::now(),
                    gso_size: None,
                };
                Ok(e.insert(conn))
            }
        }
    }

    fn handle_gateway_udp(
        &mut self,
        frame: &EthernetRepr,
        addresses: &Ipv4Addresses,
        udp: &UdpPacket<&[u8]>,
    ) -> Result<bool, DropReason> {
        match udp.dst_port() {
            DHCP_SERVER => {
                self.handle_dhcp(udp.payload())?;
                Ok(true)
            }
            DNS_PORT => self.handle_dns(
                frame,
                addresses.src_addr.into(),
                addresses.dst_addr.into(),
                udp,
            ),
            _ => Ok(false),
        }
    }

    fn handle_gateway_udp_v6(
        &mut self,
        frame: &EthernetRepr,
        addresses: &Ipv6Addresses,
        udp: &UdpPacket<&[u8]>,
    ) -> Result<bool, DropReason> {
        let payload = udp.payload();
        match udp.dst_port() {
            DHCPV6_SERVER => {
                self.handle_dhcpv6(payload, Some(addresses.src_addr))?;
                Ok(true)
            }
            DNS_PORT => self.handle_dns(
                frame,
                addresses.src_addr.into(),
                addresses.dst_addr.into(),
                udp,
            ),
            _ => Ok(false),
        }
    }

    /// Binds to the specified host IP and port for forwarding inbound UDP
    /// packets to the guest.
    pub fn bind_udp_port(&mut self, socket: Socket, guest_port: u16) -> Result<(), BindError> {
        if self.inner.udp.listeners.contains_key(&guest_port) {
            return Err(BindError::PortAlreadyBound(guest_port));
        }
        let socket: UdpSocket = socket.into();
        let socket = PolledSocket::new(self.client.driver(), socket).map_err(BindError::Io)?;
        self.inner.udp.listeners.insert(
            guest_port,
            UdpListener {
                socket: Some(socket),
                guest_port,
                stats: Default::default(),
            },
        );
        Ok(())
    }

    /// Unbinds from the specified guest port.
    pub fn unbind_udp_port(&mut self, port: u16) -> Result<(), BindError> {
        if self.inner.udp.listeners.remove(&port).is_some() {
            Ok(())
        } else {
            Err(BindError::PortNotBound)
        }
    }

    fn handle_dns(
        &mut self,
        frame: &EthernetRepr,
        src_addr: IpAddress,
        dst_addr: IpAddress,
        udp: &UdpPacket<&[u8]>,
    ) -> Result<bool, DropReason> {
        let Some(dns) = self.inner.dns.as_mut() else {
            return Ok(false);
        };

        let request = DnsRequest {
            flow: DnsFlow {
                src: SocketAddr::new(src_addr.into(), udp.src_port()),
                dst: SocketAddr::new(dst_addr.into(), udp.dst_port()),
                gateway_mac: self.inner.state.params.gateway_mac,
                client_mac: frame.src_addr,
                transport: crate::dns_resolver::DnsTransport::Udp,
            },
            dns_query: udp.payload(),
        };

        // Submit the DNS query with addressing information.
        // The response will be queued and sent later in poll_udp, unless the
        // resolver is rate-limited, in which case it returns a SERVFAIL to
        // emit immediately.
        let immediate_response = dns.submit_udp_query(&request).map_err(|e| {
            tracelimit::error_ratelimited!(error = ?e, "Failed to start DNS query");
            DropReason::Packet(smoltcp::wire::Error)
        })?;

        if let Some(response) = immediate_response {
            if let Err(e) = self.send_dns_response(&response) {
                tracelimit::error_ratelimited!(error = ?e, "Failed to send DNS SERVFAIL response");
            }
        }

        Ok(true)
    }

    fn send_dns_response(&mut self, response: &DnsResponse) -> Result<(), DropReason> {
        tracing::debug!(
            response_len = response.response_data.len(),
            src = %response.flow.src,
            dst = %response.flow.dst,
            "Sending UDP DNS response"
        );

        let buffer = &mut self.inner.state.buffer;

        // Determine header length based on IP version
        let (ip_header_len, checksum_state) = match response.flow.src.ip() {
            IpAddr::V4(_) => (IPV4_HEADER_LEN, ChecksumState::UDP4),
            IpAddr::V6(_) => (IPV6_HEADER_LEN, ChecksumState::NONE),
        };

        let payload_offset = ETHERNET_HEADER_LEN + ip_header_len + UDP_HEADER_LEN;
        let required_size = payload_offset + response.response_data.len();

        if required_size > buffer.len() {
            return Err(DropReason::SendBufferFull);
        }

        buffer[payload_offset..required_size].copy_from_slice(&response.response_data);

        let mut eth_frame = EthernetFrame::new_unchecked(&mut buffer[..]);
        let frame_len = build_udp_packet(
            &mut eth_frame,
            response.flow.dst.ip().into(),
            response.flow.src.ip().into(),
            response.flow.dst.port(),
            response.flow.src.port(),
            response.response_data.len(),
            response.flow.gateway_mac,
            response.flow.client_mac,
        );

        self.client.recv(&buffer[..frame_len], &checksum_state);

        Ok(())
    }

    #[cfg(test)]
    /// Returns the current number of active UDP connections.
    pub fn udp_connection_count(&self) -> usize {
        self.inner.udp.connections.len()
    }
}

/// Helper function to build a complete UDP packet in an Ethernet frame.
///
/// This function constructs the Ethernet, IP (v4 or v6), and UDP headers, and assumes
/// the UDP payload is already present in the buffer at the correct offset.
///
/// Returns the total length of the constructed frame.
fn build_udp_packet<T: AsRef<[u8]> + AsMut<[u8]> + ?Sized>(
    eth_frame: &mut EthernetFrame<&mut T>,
    src_ip: IpAddress,
    dst_ip: IpAddress,
    src_port: u16,
    dst_port: u16,
    payload_len: usize,
    src_mac: EthernetAddress,
    dst_mac: EthernetAddress,
) -> usize {
    // Build Ethernet header
    eth_frame.set_src_addr(src_mac);
    eth_frame.set_dst_addr(dst_mac);

    match (src_ip, dst_ip) {
        (IpAddress::Ipv4(src_ip), IpAddress::Ipv4(dst_ip)) => {
            eth_frame.set_ethertype(EthernetProtocol::Ipv4);

            // Build IPv4 header
            let mut ipv4_packet = Ipv4Packet::new_unchecked(eth_frame.payload_mut());
            let ipv4_repr = Ipv4Repr {
                src_addr: src_ip,
                dst_addr: dst_ip,
                next_header: IpProtocol::Udp,
                payload_len: UDP_HEADER_LEN + payload_len,
                hop_limit: 64,
            };
            ipv4_repr.emit(&mut ipv4_packet, &ChecksumCapabilities::default());

            // Build UDP header (payload is already in place)
            let mut udp_packet = UdpPacket::new_unchecked(ipv4_packet.payload_mut());
            udp_packet.set_src_port(src_port);
            udp_packet.set_dst_port(dst_port);
            udp_packet.set_len((UDP_HEADER_LEN + payload_len) as u16);
            udp_packet.fill_checksum(&src_ip.into(), &dst_ip.into());

            // Return total frame length
            ETHERNET_HEADER_LEN + ipv4_packet.total_len() as usize
        }
        (IpAddress::Ipv6(src_ip), IpAddress::Ipv6(dst_ip)) => {
            eth_frame.set_ethertype(EthernetProtocol::Ipv6);

            // Build IPv6 header
            let mut ipv6_packet = Ipv6Packet::new_unchecked(eth_frame.payload_mut());
            let ipv6_repr = Ipv6Repr {
                src_addr: src_ip,
                dst_addr: dst_ip,
                next_header: IpProtocol::Udp,
                payload_len: UDP_HEADER_LEN + payload_len,
                hop_limit: 64,
            };
            ipv6_repr.emit(&mut ipv6_packet);

            // Build UDP header (payload is already in place)
            let mut udp_packet = UdpPacket::new_unchecked(ipv6_packet.payload_mut());
            udp_packet.set_src_port(src_port);
            udp_packet.set_dst_port(dst_port);
            udp_packet.set_len((UDP_HEADER_LEN + payload_len) as u16);
            udp_packet.fill_checksum(&src_ip.into(), &dst_ip.into());

            // Return total frame length
            ETHERNET_HEADER_LEN + ipv6_packet.total_len()
        }
        _ => panic!("mismatched IP address families"),
    }
}

#[cfg(all(unix, test))]
mod tests {
    use super::*;
    use crate::Consomme;
    use crate::ConsommeParams;
    use pal_async::DefaultDriver;
    use parking_lot::Mutex;
    use smoltcp::wire::Ipv4Address;
    use std::sync::Arc;

    /// Mock test client that captures received packets
    struct TestClient {
        driver: Arc<DefaultDriver>,
        received_packets: Arc<Mutex<Vec<Vec<u8>>>>,
        rx_mtu: usize,
    }

    impl TestClient {
        fn new(driver: Arc<DefaultDriver>) -> Self {
            Self {
                driver,
                received_packets: Arc::new(Mutex::new(Vec::new())),
                rx_mtu: 1514, // Standard Ethernet MTU
            }
        }
    }

    impl Client for TestClient {
        fn driver(&self) -> &dyn pal_async::driver::Driver {
            &*self.driver
        }

        fn recv(&mut self, data: &[u8], _checksum: &ChecksumState) {
            self.received_packets.lock().push(data.to_vec());
        }

        fn rx_mtu(&mut self) -> usize {
            self.rx_mtu
        }
    }

    fn create_consomme_with_timeout(timeout: Duration) -> Consomme {
        let mut params = ConsommeParams::new().expect("Failed to create params");
        params.udp_timeout = timeout;
        Consomme::new(params)
    }

    #[pal_async::async_test]
    async fn test_udp_connection_timeout(driver: DefaultDriver) {
        let driver = Arc::new(driver);
        let mut consomme = create_consomme_with_timeout(Duration::from_millis(100));
        let mut client = TestClient::new(driver);

        let guest_mac = consomme.params_mut().client_mac;
        let gateway_mac = consomme.params_mut().gateway_mac;
        let guest_ip: Ipv4Address = consomme.params_mut().client_ip;
        let target_ip: Ipv4Address = Ipv4Addr::LOCALHOST;

        // Create a buffer and place the payload at the correct offset
        let payload = b"test";
        let mut buffer =
            vec![0u8; ETHERNET_HEADER_LEN + IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len()];
        buffer[ETHERNET_HEADER_LEN + IPV4_HEADER_LEN + UDP_HEADER_LEN..].copy_from_slice(payload);

        let mut eth_frame = EthernetFrame::new_unchecked(&mut buffer[..]);
        let packet_len = build_udp_packet(
            &mut eth_frame,
            IpAddress::Ipv4(guest_ip),
            IpAddress::Ipv4(target_ip),
            12345,
            54321,
            payload.len(),
            guest_mac,
            gateway_mac,
        );

        let mut access = consomme.access(&mut client);
        let _ = access.send(&buffer[..packet_len], &ChecksumState::NONE);

        let mut cx = Context::from_waker(std::task::Waker::noop());
        access.poll(&mut cx);

        assert_eq!(
            access.udp_connection_count(),
            1,
            "Connection should be created"
        );

        // Manually update the last_activity to simulate timeout
        for conn in access.inner.udp.connections.values_mut() {
            conn.last_activity = Instant::now() - Duration::from_millis(150);
        }

        // Poll should remove timed out connections
        access.poll(&mut cx);

        assert_eq!(
            access.udp_connection_count(),
            0,
            "Connection should be removed after timeout"
        );
    }

    #[pal_async::async_test]
    async fn test_udp_bind_port_forward(driver: DefaultDriver) {
        let driver = Arc::new(driver);
        let mut consomme = create_consomme_with_timeout(Duration::from_secs(30));
        let mut client = TestClient::new(driver.clone());

        // Bind a UDP listener socket on an ephemeral port.
        let socket = Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None).unwrap();
        socket
            .bind(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
            .unwrap();
        let host_addr: SocketAddr = socket.local_addr().unwrap().as_socket().unwrap();

        let guest_port = 5555;
        let packets = client.received_packets.clone();
        let mut access = consomme.access(&mut client);
        access
            .bind_udp_port(socket, guest_port)
            .expect("bind should succeed");

        assert!(
            access.inner.udp.listeners.contains_key(&guest_port),
            "listener should be registered"
        );

        // Send a UDP packet to the listener from another socket.
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.send_to(b"hello", host_addr).unwrap();

        // Poll until the forwarded packet arrives (the first poll registers
        // interest, subsequent polls receive the data).
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            std::future::poll_fn(|cx| {
                access.poll(cx);
                Poll::Ready(())
            })
            .await;

            if !packets.lock().is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for forwarded UDP packet"
            );
            pal_async::timer::PolledTimer::new(&*driver)
                .sleep(Duration::from_millis(10))
                .await;
        }

        // Verify the packet targets the guest IP and the correct guest port.
        let packets = packets.lock();
        let pkt = &packets[0];
        let eth = EthernetFrame::new_unchecked(pkt.as_slice());
        let ipv4 = Ipv4Packet::new_unchecked(eth.payload());
        let udp = UdpPacket::new_unchecked(ipv4.payload());
        assert_eq!(
            udp.dst_port(),
            guest_port,
            "forwarded packet should target the guest port"
        );
    }

    #[pal_async::async_test]
    async fn test_udp_bind_duplicate_port(driver: DefaultDriver) {
        let driver = Arc::new(driver);
        let mut consomme = create_consomme_with_timeout(Duration::from_secs(30));
        let mut client = TestClient::new(driver.clone());

        let guest_port = 6666;

        let socket1 = Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None).unwrap();
        socket1
            .bind(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
            .unwrap();

        let socket2_inst = Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None).unwrap();
        socket2_inst
            .bind(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
            .unwrap();

        let mut access = consomme.access(&mut client);
        access
            .bind_udp_port(socket1, guest_port)
            .expect("first bind should succeed");

        let err = access
            .bind_udp_port(socket2_inst, guest_port)
            .expect_err("duplicate bind should fail");
        assert!(
            matches!(err, BindError::PortAlreadyBound(_)),
            "error should be PortAlreadyBound"
        );
    }
}
