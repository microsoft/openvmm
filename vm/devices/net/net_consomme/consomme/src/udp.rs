// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Access;
use super::Client;
use super::DropReason;
use super::SocketAddress;
use super::dhcp::DHCP_SERVER;
use crate::ChecksumState;
use crate::ConsommeState;
use crate::Ipv4Addresses;
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
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Packet;
use smoltcp::wire::Ipv4Repr;
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
use std::time::Duration;
use std::time::Instant;

pub(crate) struct Udp {
    connections: HashMap<SocketAddress, UdpConnection>,
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
            resp.field_mut(&format!("{}:{}", addr.ip, addr.port), conn);
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
        dst_addr: &SocketAddress,
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
            match self.socket.as_mut().unwrap().poll_io(
                cx,
                InterestSlot::Read,
                PollEvents::IN,
                |socket| {
                    socket
                        .get()
                        .recv_from(&mut eth.payload_mut()[IPV4_HEADER_LEN + UDP_HEADER_LEN..])
                },
            ) {
                Poll::Ready(Ok((n, src_addr))) => {
                    let src_ip = if let IpAddr::V4(ip) = src_addr.ip() {
                        ip
                    } else {
                        unreachable!()
                    };
                    eth.set_ethertype(EthernetProtocol::Ipv4);
                    eth.set_src_addr(state.params.gateway_mac);
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
                    addr = %format!("{}:{}", dst_addr.ip, dst_addr.port),
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

        if addresses.dst_addr == self.inner.state.params.gateway_ip
            || addresses.dst_addr.is_broadcast()
        {
            if self.handle_gateway_udp(&udp_packet)? {
                return Ok(());
            }
        }

        let guest_addr = SocketAddress {
            ip: addresses.src_addr,
            port: udp.src_port,
        };

        let conn = self.get_or_insert(guest_addr, None, Some(frame.src_addr))?;
        match conn.socket.as_mut().unwrap().get().send_to(
            udp_packet.payload(),
            (Ipv4Addr::from(addresses.dst_addr), udp.dst_port),
        ) {
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

    /// Binds to the specified host IP and port for forwarding inbound UDP
    /// packets to the guest.
    pub fn bind_udp_port(
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

    /// Unbinds from the specified host port.
    pub fn unbind_udp_port(&mut self, port: u16) -> Result<(), DropReason> {
        let guest_addr = SocketAddress {
            ip: Ipv4Addr::UNSPECIFIED.into(),
            port,
        };
        match self.inner.udp.connections.remove(&guest_addr) {
            Some(_) => Ok(()),
            None => Err(DropReason::PortNotBound),
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
            protocol: IpProtocol::Udp,
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
            target_ip.into(),
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
