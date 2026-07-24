// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use pal_async::DefaultDriver;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::EthernetFrame;
use smoltcp::wire::EthernetProtocol;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Packet;
use smoltcp::wire::Ipv4Repr;
use smoltcp::wire::Ipv6Packet;
use smoltcp::wire::Ipv6Repr;
use smoltcp::wire::TcpPacket;
use smoltcp::wire::TcpRepr;

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
    let mut consomme = Consomme::new(ConsommeConfig::new(), ConsommeParams::new().unwrap());
    let mut client = TestClient::new(driver);
    let mut buf = vec![0u8; 1514];

    let guest_mac = consomme.config().client_mac;
    let gateway_mac = consomme.config().gateway_mac;
    let guest_ip = consomme.config().client_ip;

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
    let mut consomme = Consomme::new(ConsommeConfig::new(), ConsommeParams::new().unwrap());
    let mut client = TestClient::new(driver);
    let mut buf = vec![0u8; 1514];

    let guest_mac = consomme.config().client_mac;
    let gateway_mac = consomme.config().gateway_mac;
    let guest_ip = consomme.config().client_ip;

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
    let mut consomme = Consomme::new(ConsommeConfig::new(), ConsommeParams::new().unwrap());
    let mut client = TestClient::new(driver);
    let mut buf = vec![0u8; 1514];

    let guest_mac = consomme.config().client_mac;
    let gateway_mac = consomme.config().gateway_mac;
    let guest_ip = consomme.config().client_ip;

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
    let mut consomme = Consomme::new(ConsommeConfig::new(), {
        let mut params = ConsommeParams::new().unwrap();
        params.allow_host_local_access = true;
        params
    });
    let mut client = TestClient::new(driver);
    let mut buf = vec![0u8; 1514];

    let guest_mac = consomme.config().client_mac;
    let gateway_mac = consomme.config().gateway_mac;
    let guest_ip = consomme.config().client_ip;

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
    let mut config = ConsommeConfig::new();
    config.skip_ipv6_checks = true;
    let mut consomme = Consomme::new(config, ConsommeParams::new().unwrap());
    let mut client = TestClient::new(driver);
    let mut buf = vec![0u8; 1514];

    let guest_mac = consomme.config().client_mac;
    let gateway_mac = consomme.config().gateway_mac_ipv6;
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
    let mut config = ConsommeConfig::new();
    config.skip_ipv6_checks = true;
    let mut consomme = Consomme::new(config, ConsommeParams::new().unwrap());
    let mut client = TestClient::new(driver);
    let mut buf = vec![0u8; 1514];

    let guest_mac = consomme.config().client_mac;
    let gateway_mac = consomme.config().gateway_mac_ipv6;
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
    let mut consomme = Consomme::new(ConsommeConfig::new(), ConsommeParams::new().unwrap());
    let mut client = TestClient::new(driver);
    let mut buf = vec![0u8; 1514];

    let guest_mac = consomme.config().client_mac;
    let gateway_mac = consomme.config().gateway_mac;
    let guest_ip = consomme.config().client_ip;

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

fn create_ipv4_tcp_connection(consomme: &mut Consomme, client: &mut TestClient) {
    let mut buf = vec![0u8; 1514];
    let config = consomme.config();
    let len = build_ipv4_syn(
        &mut buf,
        config.client_mac,
        config.gateway_mac,
        config.client_ip,
        Ipv4Address::new(8, 8, 8, 8),
    );

    consomme
        .access(client)
        .send(&buf[..len], &ChecksumState::NONE)
        .expect("TCP SYN should create a connection");
    assert_eq!(consomme.shard.tcp.connection_count(), 1);
}

#[pal_async::async_test]
async fn live_parameter_update_preserves_existing_connection(driver: DefaultDriver) {
    let mut consomme = Consomme::new(ConsommeConfig::new(), ConsommeParams::new().unwrap());
    let mut client = TestClient::new(driver);
    create_ipv4_tcp_connection(&mut consomme, &mut client);

    consomme.update_params(|params| params.allow_host_local_access = true);

    assert_eq!(consomme.shard.tcp.connection_count(), 1);
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

fn eui64_routable_address(config: &ConsommeConfig) -> Ipv6Address {
    let mut octets = ConsommeConfig::compute_link_local_address(config.client_mac).octets();
    octets[..8].copy_from_slice(&[0xfd, 0x00, 0x0d, 0xb8, 0, 0, 0, 0]);
    Ipv6Address::from_octets(octets)
}

fn test_runtime() -> (ConsommeConfig, ConsommePrimaryRuntime) {
    (
        ConsommeConfig::new(),
        ConsommePrimaryRuntime {
            local_addr_map: local_addr_map::LocalAddrMap::new(),
            client_ip_ipv6: None,
            client_ip_ipv6_routable: None,
        },
    )
}

#[test]
fn infer_client_link_local_from_routable_with_matching_eui64_iid() {
    let (config, mut runtime) = test_runtime();
    let expected_link_local = ConsommeConfig::compute_link_local_address(config.client_mac);
    let routable = eui64_routable_address(&config);

    runtime.infer_client_link_local_from_routable(&config, routable, "test");

    assert_eq!(runtime.client_ip_ipv6, Some(expected_link_local));
}

#[test]
fn infer_client_link_local_from_routable_ignores_privacy_iid() {
    let (config, mut runtime) = test_runtime();
    let privacy_address = Ipv6Address::new(0xfd00, 0x0db8, 0, 0, 1, 2, 3, 4);

    runtime.infer_client_link_local_from_routable(&config, privacy_address, "test");

    assert_eq!(runtime.client_ip_ipv6, None);
}

#[test]
fn infer_client_link_local_from_routable_does_not_overwrite_existing_address() {
    let (config, mut runtime) = test_runtime();
    let existing_address = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 0x1234);
    runtime.client_ip_ipv6 = Some(existing_address);
    let routable = eui64_routable_address(&config);

    runtime.infer_client_link_local_from_routable(&config, routable, "test");

    assert_eq!(runtime.client_ip_ipv6, Some(existing_address));
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

fn learn_from_ipv6_traffic(consomme: &mut Consomme, src_addr: Ipv6Address) {
    let gateway_ip = consomme.config().gateway_link_local_ipv6;
    let mut client = NoDriverClient;
    let frame = EthernetRepr {
        src_addr: consomme.config().client_mac,
        dst_addr: consomme.config().gateway_mac_ipv6,
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
}

#[test]
fn handle_ipv6_updates_link_local_from_traffic() {
    let mut config = ConsommeConfig::new();
    config.skip_ipv6_checks = true;
    let mut params = ConsommeParams::new().unwrap();
    params.allow_host_local_access = true;
    let mut consomme = Consomme::new(config, params);
    let first_address = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
    let second_address = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2);

    learn_from_ipv6_traffic(&mut consomme, first_address);
    learn_from_ipv6_traffic(&mut consomme, second_address);

    assert_eq!(
        consomme.primary.runtime.client_ip_ipv6,
        Some(second_address)
    );
}

#[test]
fn update_params_preserves_runtime_state_for_unrelated_changes() {
    let mut config = ConsommeConfig::new();
    config.skip_ipv6_checks = true;
    let mut consomme = Consomme::new(config, ConsommeParams::new().unwrap());
    let learned_address = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
    consomme.primary.runtime.client_ip_ipv6 = Some(learned_address);
    let virtual_address = consomme
        .primary
        .runtime
        .local_addr_map
        .get_or_allocate_v4(
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::new(10, 0, 0, 0),
            consomme.config().net_mask,
            consomme.config().gateway_ip,
            consomme.config().client_ip,
        )
        .unwrap();

    consomme.update_params(|params| params.allow_host_local_access = true);

    assert!(consomme.primary.config.params.allow_host_local_access);
    assert_eq!(
        consomme.primary.runtime.client_ip_ipv6,
        Some(learned_address)
    );
    assert_eq!(
        consomme
            .primary
            .runtime
            .local_addr_map
            .resolve_virtual(&std::net::IpAddr::V4(virtual_address)),
        Some(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST))
    );
}

