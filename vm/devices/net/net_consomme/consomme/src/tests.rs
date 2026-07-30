// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use pal_async::DefaultDriver;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::DnsQueryType;
use smoltcp::wire::EthernetFrame;
use smoltcp::wire::EthernetProtocol;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Packet;
use smoltcp::wire::Ipv4Repr;
use smoltcp::wire::Ipv6Packet;
use smoltcp::wire::Ipv6Repr;
use smoltcp::wire::TcpPacket;
use smoltcp::wire::TcpRepr;
use smoltcp::wire::UDP_HEADER_LEN;
use smoltcp::wire::UdpPacket;

const ETHERNET_HEADER_LEN: usize = 14;

struct TestClient {
    driver: DefaultDriver,
}

impl TestClient {
    fn new(driver: DefaultDriver) -> Self {
        Self { driver }
    }
}

impl Client for TestClient {
    fn driver(&self) -> &dyn Driver {
        &self.driver
    }

    fn recv(&mut self, _data: &[u8], _checksum: &ChecksumState) {}

    fn rx_mtu(&mut self) -> usize {
        1514
    }
}

/// Build a minimal TCP SYN packet inside an Ethernet/IPv4 frame.
fn build_ipv4_syn(
    buf: &mut [u8],
    src_mac: EthernetAddress,
    dst_mac: EthernetAddress,
    src_ip: Ipv4Address,
    dst_ip: Ipv4Address,
) -> usize {
    let tcp = TcpRepr {
        src_port: 44444,
        dst_port: 80,
        control: smoltcp::wire::TcpControl::Syn,
        seq_number: smoltcp::wire::TcpSeqNumber(1000),
        ack_number: None,
        window_len: 64240,
        window_scale: Some(7),
        max_seg_size: Some(1460),
        sack_permitted: false,
        sack_ranges: [None, None, None],
        timestamp: None,
        payload: &[],
    };

    let mut eth = EthernetFrame::new_unchecked(buf);
    eth.set_src_addr(src_mac);
    eth.set_dst_addr(dst_mac);
    eth.set_ethertype(EthernetProtocol::Ipv4);

    let ip_repr = Ipv4Repr {
        src_addr: src_ip,
        dst_addr: dst_ip,
        next_header: IpProtocol::Tcp,
        payload_len: tcp.header_len(),
        hop_limit: 64,
    };
    let mut ipv4 = Ipv4Packet::new_unchecked(eth.payload_mut());
    ip_repr.emit(&mut ipv4, &ChecksumCapabilities::default());

    let mut tcp_pkt = TcpPacket::new_unchecked(ipv4.payload_mut());
    tcp.emit(
        &mut tcp_pkt,
        &src_ip.into(),
        &dst_ip.into(),
        &ChecksumCapabilities::default(),
    );
    tcp_pkt.fill_checksum(&src_ip.into(), &dst_ip.into());

    ETHERNET_HEADER_LEN + ipv4.total_len() as usize
}

/// Build a minimal TCP SYN packet inside an Ethernet/IPv6 frame.
fn build_ipv6_syn(
    buf: &mut [u8],
    src_mac: EthernetAddress,
    dst_mac: EthernetAddress,
    src_ip: Ipv6Address,
    dst_ip: Ipv6Address,
) -> usize {
    let tcp = TcpRepr {
        src_port: 44444,
        dst_port: 80,
        control: smoltcp::wire::TcpControl::Syn,
        seq_number: smoltcp::wire::TcpSeqNumber(1000),
        ack_number: None,
        window_len: 64240,
        window_scale: Some(7),
        max_seg_size: Some(1460),
        sack_permitted: false,
        sack_ranges: [None, None, None],
        timestamp: None,
        payload: &[],
    };

    let mut eth = EthernetFrame::new_unchecked(buf);
    eth.set_src_addr(src_mac);
    eth.set_dst_addr(dst_mac);
    eth.set_ethertype(EthernetProtocol::Ipv6);

    let ip_repr = Ipv6Repr {
        src_addr: src_ip,
        dst_addr: dst_ip,
        next_header: IpProtocol::Tcp,
        payload_len: tcp.header_len(),
        hop_limit: 64,
    };
    let mut ipv6 = Ipv6Packet::new_unchecked(eth.payload_mut());
    ip_repr.emit(&mut ipv6);

    let mut tcp_pkt = TcpPacket::new_unchecked(ipv6.payload_mut());
    tcp.emit(
        &mut tcp_pkt,
        &src_ip.into(),
        &dst_ip.into(),
        &ChecksumCapabilities::default(),
    );
    tcp_pkt.fill_checksum(&src_ip.into(), &dst_ip.into());

    ETHERNET_HEADER_LEN + smoltcp::wire::IPV6_HEADER_LEN + tcp.header_len()
}

/// Verify that traffic to IPv4 loopback (127.0.0.1) is blocked by default.
#[pal_async::async_test]
async fn ipv4_loopback_blocked_by_default(driver: DefaultDriver) {
    let mut consomme = Consomme::new(ConsommeParams::new().unwrap());
    let mut client = TestClient::new(driver);
    let mut buf = vec![0u8; 1514];

    let guest_mac = consomme.params_mut().client_mac;
    let gateway_mac = consomme.params_mut().gateway_mac;
    let guest_ip = consomme.params_mut().client_ip;

    let len = build_ipv4_syn(
        &mut buf,
        guest_mac,
        gateway_mac,
        guest_ip,
        Ipv4Address::new(127, 0, 0, 1),
    );
    let result = consomme
        .access(&mut client)
        .send(&buf[..len], &ChecksumState::NONE);
    assert!(
        matches!(result, Err(DropReason::DestinationNotAllowed)),
        "loopback traffic should be rejected, got {result:?}"
    );
}

/// Verify that traffic to IPv4 unspecified (0.0.0.0) is blocked.
#[pal_async::async_test]
async fn ipv4_unspecified_blocked(driver: DefaultDriver) {
    let mut consomme = Consomme::new(ConsommeParams::new().unwrap());
    let mut client = TestClient::new(driver);
    let mut buf = vec![0u8; 1514];

    let guest_mac = consomme.params_mut().client_mac;
    let gateway_mac = consomme.params_mut().gateway_mac;
    let guest_ip = consomme.params_mut().client_ip;

    let len = build_ipv4_syn(
        &mut buf,
        guest_mac,
        gateway_mac,
        guest_ip,
        Ipv4Address::new(0, 0, 0, 0),
    );
    let result = consomme
        .access(&mut client)
        .send(&buf[..len], &ChecksumState::NONE);
    assert!(
        matches!(result, Err(DropReason::DestinationNotAllowed)),
        "unspecified address traffic should be rejected, got {result:?}"
    );
}

/// Verify that traffic to IPv4 link-local (169.254.x.x) is blocked.
#[pal_async::async_test]
async fn ipv4_link_local_blocked(driver: DefaultDriver) {
    let mut consomme = Consomme::new(ConsommeParams::new().unwrap());
    let mut client = TestClient::new(driver);
    let mut buf = vec![0u8; 1514];

    let guest_mac = consomme.params_mut().client_mac;
    let gateway_mac = consomme.params_mut().gateway_mac;
    let guest_ip = consomme.params_mut().client_ip;

    let len = build_ipv4_syn(
        &mut buf,
        guest_mac,
        gateway_mac,
        guest_ip,
        Ipv4Address::new(169, 254, 1, 1),
    );
    let result = consomme
        .access(&mut client)
        .send(&buf[..len], &ChecksumState::NONE);
    assert!(
        matches!(result, Err(DropReason::DestinationNotAllowed)),
        "link-local traffic should be rejected, got {result:?}"
    );
}

/// Verify that loopback traffic is allowed when opted in.
#[pal_async::async_test]
async fn ipv4_loopback_allowed_when_opted_in(driver: DefaultDriver) {
    let mut consomme = Consomme::new({
        let mut params = ConsommeParams::new().unwrap();
        params.allow_host_local_access = true;
        params
    });
    let mut client = TestClient::new(driver);
    let mut buf = vec![0u8; 1514];

    let guest_mac = consomme.params_mut().client_mac;
    let gateway_mac = consomme.params_mut().gateway_mac;
    let guest_ip = consomme.params_mut().client_ip;

    let len = build_ipv4_syn(
        &mut buf,
        guest_mac,
        gateway_mac,
        guest_ip,
        Ipv4Address::new(127, 0, 0, 1),
    );
    let result = consomme
        .access(&mut client)
        .send(&buf[..len], &ChecksumState::NONE);
    // Should not be DestinationNotAllowed (may fail for other reasons
    // like no listener, but that's fine).
    assert!(
        !matches!(result, Err(DropReason::DestinationNotAllowed)),
        "loopback traffic should be allowed when opted in, got {result:?}"
    );
}

/// Verify that traffic to IPv6 loopback (::1) is blocked by default.
#[pal_async::async_test]
async fn ipv6_loopback_blocked_by_default(driver: DefaultDriver) {
    let mut consomme = Consomme::new({
        let mut params = ConsommeParams::new().unwrap();
        params.skip_ipv6_checks = true;
        params
    });
    let mut client = TestClient::new(driver);
    let mut buf = vec![0u8; 1514];

    let guest_mac = consomme.params_mut().client_mac;
    let gateway_mac = consomme.params_mut().gateway_mac_ipv6;
    let guest_ip = Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2);

    let len = build_ipv6_syn(
        &mut buf,
        guest_mac,
        gateway_mac,
        guest_ip,
        Ipv6Address::new(0, 0, 0, 0, 0, 0, 0, 1),
    );
    let result = consomme
        .access(&mut client)
        .send(&buf[..len], &ChecksumState::NONE);
    assert!(
        matches!(result, Err(DropReason::DestinationNotAllowed)),
        "IPv6 loopback traffic should be rejected, got {result:?}"
    );
}

/// Verify that traffic to IPv6 link-local (fe80::/10) is blocked by default.
#[pal_async::async_test]
async fn ipv6_link_local_blocked_by_default(driver: DefaultDriver) {
    let mut consomme = Consomme::new({
        let mut params = ConsommeParams::new().unwrap();
        params.skip_ipv6_checks = true;
        params
    });
    let mut client = TestClient::new(driver);
    let mut buf = vec![0u8; 1514];

    let guest_mac = consomme.params_mut().client_mac;
    let gateway_mac = consomme.params_mut().gateway_mac_ipv6;
    let guest_ip = Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2);

    let len = build_ipv6_syn(
        &mut buf,
        guest_mac,
        gateway_mac,
        guest_ip,
        Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1),
    );
    let result = consomme
        .access(&mut client)
        .send(&buf[..len], &ChecksumState::NONE);
    assert!(
        matches!(result, Err(DropReason::DestinationNotAllowed)),
        "IPv6 link-local traffic should be rejected, got {result:?}"
    );
}

/// Verify that traffic to a normal external IP is not blocked.
#[pal_async::async_test]
async fn ipv4_normal_destination_not_blocked(driver: DefaultDriver) {
    let mut consomme = Consomme::new(ConsommeParams::new().unwrap());
    let mut client = TestClient::new(driver);
    let mut buf = vec![0u8; 1514];

    let guest_mac = consomme.params_mut().client_mac;
    let gateway_mac = consomme.params_mut().gateway_mac;
    let guest_ip = consomme.params_mut().client_ip;

    let len = build_ipv4_syn(
        &mut buf,
        guest_mac,
        gateway_mac,
        guest_ip,
        Ipv4Address::new(8, 8, 8, 8),
    );
    let result = consomme
        .access(&mut client)
        .send(&buf[..len], &ChecksumState::NONE);
    // Should not be DestinationNotAllowed (may fail for other reasons).
    assert!(
        !matches!(result, Err(DropReason::DestinationNotAllowed)),
        "normal destination should not be blocked, got {result:?}"
    );
}

#[test]
fn test_is_same_ipv6_subnet_basic() {
    let a = Ipv6Address::new(0x2001, 0x0db8, 0x0001, 0, 0, 0, 0, 1);
    let b = Ipv6Address::new(0x2001, 0x0db8, 0x0001, 0, 0, 0, 0, 2);
    assert!(is_same_ipv6_subnet(a, b, 48));
    assert!(!is_same_ipv6_subnet(a, b, 128));
}

#[test]
fn test_is_same_ipv6_subnet_prefix_zero() {
    let a = Ipv6Address::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1);
    let b = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
    assert!(is_same_ipv6_subnet(a, b, 0));
}

#[test]
fn test_is_same_ipv6_subnet_prefix_128_exact_match() {
    let a = Ipv6Address::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1);
    assert!(is_same_ipv6_subnet(a, a, 128));
}

#[test]
fn test_is_same_ipv6_subnet_prefix_128_no_match() {
    let a = Ipv6Address::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1);
    let b = Ipv6Address::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 2);
    assert!(!is_same_ipv6_subnet(a, b, 128));
}

#[test]
fn test_is_same_ipv6_subnet_prefix_above_128_does_not_panic() {
    let a = Ipv6Address::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1);
    let b = Ipv6Address::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 2);
    // prefix_len > 128 should behave like /128 (exact match), not panic.
    assert!(is_same_ipv6_subnet(a, a, 200));
    assert!(!is_same_ipv6_subnet(a, b, 255));
}

fn eui64_routable_address(params: &ConsommeParams) -> Ipv6Address {
    let mut octets = ConsommeParams::compute_link_local_address(params.client_mac).octets();
    octets[..8].copy_from_slice(&[0xfd, 0x00, 0x0d, 0xb8, 0, 0, 0, 0]);
    Ipv6Address::from_octets(octets)
}

#[test]
fn infer_client_link_local_from_routable_with_matching_eui64_iid() {
    let mut params = ConsommeParams::new().unwrap();
    params.client_ip_ipv6 = None;
    let expected_link_local = ConsommeParams::compute_link_local_address(params.client_mac);

    params.infer_client_link_local_from_routable(eui64_routable_address(&params), "test");

    assert_eq!(params.client_ip_ipv6, Some(expected_link_local));
}

#[test]
fn infer_client_link_local_from_routable_ignores_privacy_iid() {
    let mut params = ConsommeParams::new().unwrap();
    params.client_ip_ipv6 = None;
    let privacy_address = Ipv6Address::new(0xfd00, 0x0db8, 0, 0, 1, 2, 3, 4);

    params.infer_client_link_local_from_routable(privacy_address, "test");

    assert_eq!(params.client_ip_ipv6, None);
}

#[test]
fn infer_client_link_local_from_routable_does_not_overwrite_existing_address() {
    let mut params = ConsommeParams::new().unwrap();
    let existing_address = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 0x1234);
    params.client_ip_ipv6 = Some(existing_address);

    params.infer_client_link_local_from_routable(eui64_routable_address(&params), "test");

    assert_eq!(params.client_ip_ipv6, Some(existing_address));
}

/// A minimal Client implementation for synchronous tests that do not create
/// new connections and therefore never call `driver()`.
struct NoDriverClient;

impl Client for NoDriverClient {
    fn driver(&self) -> &dyn Driver {
        unreachable!("IPv6 address learning tests do not use the client driver")
    }

    fn recv(&mut self, _data: &[u8], _checksum: &ChecksumState) {}

    fn rx_mtu(&mut self) -> usize {
        MIN_MTU
    }
}

fn learn_from_ipv6_traffic(params: &mut ConsommeParams, src_addr: Ipv6Address) {
    params.skip_ipv6_checks = true;
    params.allow_host_local_access = true;
    let gateway_ip = params.gateway_link_local_ipv6;
    let mut consomme = Consomme::new(std::mem::replace(params, ConsommeParams::new().unwrap()));
    let mut client = NoDriverClient;
    let frame = EthernetRepr {
        src_addr: consomme.state.params.client_mac,
        dst_addr: consomme.state.params.gateway_mac_ipv6,
        ethertype: EthernetProtocol::Ipv6,
    };
    let mut payload = [0; smoltcp::wire::IPV6_HEADER_LEN];
    Ipv6Repr {
        src_addr,
        dst_addr: gateway_ip,
        next_header: IpProtocol::Tcp,
        payload_len: 0,
        hop_limit: 64,
    }
    .emit(&mut Ipv6Packet::new_unchecked(&mut payload));

    let _ = consomme
        .access(&mut client)
        .handle_ipv6(&frame, &payload, &ChecksumState::TCP6);
    *params = consomme.state.params;
}

#[test]
fn handle_ipv6_updates_link_local_from_traffic() {
    let mut params = ConsommeParams::new().unwrap();
    params.client_ip_ipv6 = None;
    let first_address = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
    let second_address = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2);

    learn_from_ipv6_traffic(&mut params, first_address);
    learn_from_ipv6_traffic(&mut params, second_address);

    assert_eq!(params.client_ip_ipv6, Some(second_address));
}

#[test]
fn create_virtual_address_allocates_subnet_address() {
    let mut consomme = Consomme::new(ConsommeParams::new().unwrap());

    // Expect 10.0.0.254 since the default subnet is 10.0.0/24.
    let addr = consomme
        .create_virtual_address(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .unwrap();
    assert_eq!(addr, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 254)));

    // Requesting a virtual address for the same destination returns the same
    // address.
    let addr_again = consomme
        .create_virtual_address(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .unwrap();
    assert_eq!(addr, addr_again);

    // Validate that a different destination gets a different address.
    let other = consomme
        .create_virtual_address(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)))
        .unwrap();
    assert_eq!(other, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 253)));
}

#[test]
fn create_virtual_address_allocates_ipv6_link_local() {
    let mut consomme = Consomme::new(ConsommeParams::new().unwrap());

    let addr = consomme
        .create_virtual_address(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST))
        .unwrap();
    // IPv6 virtual addresses are allocated from the fe80::ff:fe00:NNNN:1 range.
    assert_eq!(
        addr,
        IpAddr::V6(std::net::Ipv6Addr::new(
            0xfe80, 0, 0, 0, 0x00ff, 0xfe00, 0x0001, 0x0001
        ))
    );
}

/// Build a DNS `A`-record query for `name` (transaction id `id`, RD=1).
fn build_dns_a_query(id: u16, name: &str) -> Vec<u8> {
    let mut q = Vec::new();
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00]); // flags: RD=1
    q.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    q.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // ANCOUNT/NSCOUNT/ARCOUNT = 0

    for label in name.split('.').filter(|l| !l.is_empty()) {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }

    q.push(0); // root label
    q.extend_from_slice(&u16::from(DnsQueryType::A).to_be_bytes()); // QTYPE = A
    q.extend_from_slice(&1u16.to_be_bytes()); // QCLASS = IN
    q
}

/// Build an Ethernet/IPv4/UDP frame carrying `dns_payload` from the guest to
/// the gateway's DNS port (53). Returns the total frame length.
fn build_ipv4_dns_query(
    buf: &mut [u8],
    src_mac: EthernetAddress,
    dst_mac: EthernetAddress,
    src_ip: Ipv4Address,
    dst_ip: Ipv4Address,
    src_port: u16,
    dns_payload: &[u8],
) -> usize {
    let mut eth = EthernetFrame::new_unchecked(buf);
    eth.set_src_addr(src_mac);
    eth.set_dst_addr(dst_mac);
    eth.set_ethertype(EthernetProtocol::Ipv4);

    let ip_repr = Ipv4Repr {
        src_addr: src_ip,
        dst_addr: dst_ip,
        next_header: IpProtocol::Udp,
        payload_len: UDP_HEADER_LEN + dns_payload.len(),
        hop_limit: 64,
    };
    let mut ipv4 = Ipv4Packet::new_unchecked(eth.payload_mut());
    ip_repr.emit(&mut ipv4, &ChecksumCapabilities::default());

    let mut udp = UdpPacket::new_unchecked(ipv4.payload_mut());
    udp.set_src_port(src_port);
    udp.set_dst_port(DNS_PORT);
    udp.set_len((UDP_HEADER_LEN + dns_payload.len()) as u16);
    udp.payload_mut().copy_from_slice(dns_payload);
    udp.fill_checksum(&src_ip.into(), &dst_ip.into());

    ETHERNET_HEADER_LEN + ipv4.total_len() as usize
}

/// A [`Client`] that records every frame consomme delivers to the guest.
struct CapturingClient {
    driver: DefaultDriver,
    received: Vec<Vec<u8>>,
}

impl CapturingClient {
    fn new(driver: DefaultDriver) -> Self {
        Self {
            driver,
            received: Vec::new(),
        }
    }
}

impl Client for CapturingClient {
    fn driver(&self) -> &dyn Driver {
        &self.driver
    }

    fn recv(&mut self, data: &[u8], _checksum: &ChecksumState) {
        self.received.push(data.to_vec());
    }

    fn rx_mtu(&mut self) -> usize {
        1514
    }
}

/// End to end validation for a static DNS A record.
#[pal_async::async_test]
async fn static_dns_a_record_answered(driver: DefaultDriver) {
    let mut consomme = Consomme::new(ConsommeParams::new().unwrap());
    consomme
        .add_dns_record(StaticDnsRecordType::A, "example.com", &[10, 0, 0, 5])
        .unwrap();

    let guest_mac = consomme.params_mut().client_mac;
    let gateway_mac = consomme.params_mut().gateway_mac;
    let guest_ip = consomme.params_mut().client_ip;
    let gateway_ip = consomme.params_mut().gateway_ip;

    let query = build_dns_a_query(0x1234, "example.com");
    let query_src_port = 40000u16;
    let mut buf = vec![0u8; 1514];
    let len = build_ipv4_dns_query(
        &mut buf,
        guest_mac,
        gateway_mac,
        guest_ip,
        gateway_ip,
        query_src_port,
        &query,
    );

    let mut client = CapturingClient::new(driver);
    consomme
        .access(&mut client)
        .send(&buf[..len], &ChecksumState::NONE)
        .expect("static DNS query should be handled");

    // Exactly one response frame should have been delivered to the guest.
    assert_eq!(client.received.len(), 1, "expected one DNS response frame");

    // Parse the Ethernet/IPv4/UDP framing back off the wire.
    let eth = EthernetFrame::new_checked(client.received[0].as_slice()).unwrap();
    assert_eq!(eth.ethertype(), EthernetProtocol::Ipv4);
    assert_eq!(eth.src_addr(), gateway_mac);
    assert_eq!(eth.dst_addr(), guest_mac);

    let ipv4 = Ipv4Packet::new_checked(eth.payload()).unwrap();
    assert_eq!(ipv4.next_header(), IpProtocol::Udp);
    assert_eq!(ipv4.src_addr(), gateway_ip);
    assert_eq!(ipv4.dst_addr(), guest_ip);

    let udp = UdpPacket::new_checked(ipv4.payload()).unwrap();
    assert_eq!(udp.src_port(), DNS_PORT, "answered from the DNS port");
    assert_eq!(
        udp.dst_port(),
        query_src_port,
        "back to the query source port"
    );

    // Validate the DNS answer itself.
    let dns = udp.payload();
    assert_eq!(&dns[0..2], &[0x12, 0x34], "transaction id echoed");
    assert_eq!(dns[2], 0x85, "QR + AA + RD set");
    assert_eq!(dns[3], 0x80, "RA set, RCODE 0");
    assert_eq!(
        u16::from_be_bytes([dns[6], dns[7]]),
        1,
        "exactly one answer"
    );
    assert_eq!(&dns[dns.len() - 4..], &[10, 0, 0, 5], "answer address");
    assert_eq!(
        u16::from_be_bytes([dns[dns.len() - 6], dns[dns.len() - 5]]),
        4,
        "RDLENGTH == 4"
    );
}
