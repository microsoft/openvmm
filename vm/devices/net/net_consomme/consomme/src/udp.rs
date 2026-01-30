// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Access;
use super::Client;
use super::DropReason;
use super::dhcp::DHCP_SERVER;
use super::dhcpv6::DHCPV6_SERVER;
use crate::ChecksumState;
use crate::ConsommeState;
use crate::IpAddresses;
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
use smoltcp::wire::IpRepr;
use smoltcp::wire::Ipv4Packet;
use smoltcp::wire::Ipv6Address;
use smoltcp::wire::Ipv6Packet;
use smoltcp::wire::UDP_HEADER_LEN;
use smoltcp::wire::UdpPacket;
use smoltcp::wire::UdpRepr;
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

pub(crate) struct Udp {
    connections: HashMap<SocketAddr, UdpConnection>,
    timeout: Duration,
}

impl Udp {
    pub fn new(timeout: Duration) -> Self {
        Self {
            connections: HashMap::new(),
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
    #[inspect(debug)]
    last_activity: Instant,
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
                    eth.set_dst_addr(self.guest_mac);
                    eth.set_src_addr(state.params.gateway_mac);
                    let ip = IpRepr::new(
                        src_addr.ip().into(),
                        dst_addr.ip().into(),
                        IpProtocol::Udp,
                        UDP_HEADER_LEN + n,
                        64,
                    );

                    match ip {
                        IpRepr::Ipv4(_) => eth.set_ethertype(EthernetProtocol::Ipv4),
                        IpRepr::Ipv6(_) => eth.set_ethertype(EthernetProtocol::Ipv6),
                    }

                    let ip_packet_buf = eth.payload_mut();
                    ip.emit(&mut *ip_packet_buf, &ChecksumCapabilities::default());
                    let (udp_payload_buf, ip_total_len) = match dst_addr {
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

                    let dst_ip_addr: IpAddress = dst_addr.ip().into();
                    let src_ip_addr: IpAddress = src_addr.ip().into();
                    let mut udp_packet = UdpPacket::new_unchecked(udp_payload_buf);
                    udp_packet.set_src_port(src_addr.port());
                    udp_packet.set_dst_port(dst_addr.port());
                    udp_packet.set_len((UDP_HEADER_LEN + n) as u16);
                    udp_packet.fill_checksum(&src_ip_addr, &dst_ip_addr);

                    let packet_len = ETHERNET_HEADER_LEN + ip_total_len;
                    let checksum_state = match dst_addr {
                        SocketAddr::V4(_) => ChecksumState::UDP4,
                        SocketAddr::V6(_) => ChecksumState::NONE,
                    };

                    // Send packet to client
                    client.recv(&eth.as_ref()[..packet_len], &checksum_state);
                    self.stats.rx_packets.increment();
                    self.last_activity = Instant::now();
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
        let timeout = self.inner.udp.timeout;
        let now = Instant::now();

        self.inner.udp.connections.retain(|dst_addr, conn| {
            // Check if connection has timed out
            if now.duration_since(conn.last_activity) > timeout {
                tracing::warn!(
                    addr = %format!("{}:{}", dst_addr.ip(), dst_addr.port()),
                    "UDP connection timed out"
                );
                return false;
            }

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
                let dst_octets = addrs.dst_addr.octets();
                if addrs.dst_addr == self.inner.state.params.gateway_link_local_ipv6
                    || dst_octets[0..2] == [0xff, 0x02]
                {
                    if self.handle_gateway_udp_v6(&udp_packet, Some(addrs.src_addr))? {
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
        guest_mac: Option<EthernetAddress>,
    ) -> Result<&mut UdpConnection, DropReason> {
        let entry = self.inner.udp.connections.entry(guest_addr);
        match entry {
            hash_map::Entry::Occupied(conn) => Ok(conn.into_mut()),
            hash_map::Entry::Vacant(e) => {
                let bind_addr: SocketAddr = match guest_addr {
                    SocketAddr::V4(_) => {
                        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
                    }
                    SocketAddr::V6(_) => {
                        SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0))
                    }
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

    fn handle_gateway_udp_v6(
        &mut self,
        udp: &UdpPacket<&[u8]>,
        client_ip: Option<Ipv6Address>,
    ) -> Result<bool, DropReason> {
        let payload = udp.payload();
        match udp.dst_port() {
            DHCPV6_SERVER => {
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
            Some(IpAddr::V4(ip)) => SocketAddr::V4(SocketAddrV4::new(ip, port)),
            Some(IpAddr::V6(ip)) => SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0)),
            None => SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)),
        };
        let _ = self.get_or_insert(guest_addr, None)?;
        Ok(())
    }

    /// Unbinds from the specified host port for both IPv4 and IPv6.
    pub fn unbind_udp_port(&mut self, port: u16) -> Result<(), DropReason> {
        // Try to remove both IPv4 and IPv6 bindings
        let v4_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port));
        let v6_addr = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0));

        let v4_removed = self.inner.udp.connections.remove(&v4_addr).is_some();
        let v6_removed = self.inner.udp.connections.remove(&v6_addr).is_some();

        if v4_removed || v6_removed {
            Ok(())
        } else {
            Err(DropReason::PortNotBound)
        }
    }

    #[cfg(test)]
    /// Returns the current number of active UDP connections.
    pub fn udp_connection_count(&self) -> usize {
        self.inner.udp.connections.len()
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
    use smoltcp::wire::Ipv4Repr;
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

    /// Helper to build an Ethernet/IPv4/UDP packet
    fn build_udp_packet(
        guest_mac: EthernetAddress,
        gateway_mac: EthernetAddress,
        guest_ip: Ipv4Address,
        external_ip: Ipv4Address,
        guest_port: u16,
        external_port: u16,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut buffer =
            vec![0u8; ETHERNET_HEADER_LEN + IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len()];

        let mut eth = EthernetFrame::new_unchecked(&mut buffer);
        eth.set_src_addr(guest_mac);
        eth.set_dst_addr(gateway_mac);
        eth.set_ethertype(EthernetProtocol::Ipv4);

        let mut ipv4 = Ipv4Packet::new_unchecked(eth.payload_mut());
        Ipv4Repr {
            src_addr: guest_ip,
            dst_addr: external_ip,
            next_header: IpProtocol::Udp,
            payload_len: UDP_HEADER_LEN + payload.len(),
            hop_limit: 64,
        }
        .emit(&mut ipv4, &ChecksumCapabilities::default());

        let mut udp = UdpPacket::new_unchecked(ipv4.payload_mut());
        UdpRepr {
            src_port: guest_port,
            dst_port: external_port,
        }
        .emit(
            &mut udp,
            &guest_ip.into(),
            &external_ip.into(),
            payload.len(),
            |buf| buf.copy_from_slice(payload),
            &ChecksumCapabilities::default(),
        );

        buffer
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
        let guest_ip = consomme.params_mut().client_ip;
        let target_ip = Ipv4Addr::LOCALHOST;

        let packet = build_udp_packet(
            guest_mac,
            gateway_mac,
            guest_ip,
            target_ip,
            12345,
            54321,
            b"test",
        );

        let mut access = consomme.access(&mut client);
        let _ = access.send(&packet, &ChecksumState::NONE);

        #[allow(clippy::disallowed_methods)]
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
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
}
