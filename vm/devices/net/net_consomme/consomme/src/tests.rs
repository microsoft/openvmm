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
        params.allow_guest_loopback_access = true;
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
