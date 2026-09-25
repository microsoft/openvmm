// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of the exact static IPv4 identity and the gateway loopback
//! translation.

use super::*;

#[test]
fn static_ipv4_identity_is_exact_and_disables_ipv6_advertisement() {
    let mut params = ConsommeParams::new().unwrap();
    params
        .set_static_ipv4(
            Ipv4Addr::new(192, 168, 5, 37),
            28,
            Ipv4Addr::new(192, 168, 5, 33),
            [0x52, 0x54, 0, 168, 5, 33],
        )
        .unwrap();

    assert_eq!(params.client_ip, Ipv4Address::new(192, 168, 5, 37));
    assert_eq!(params.gateway_ip, Ipv4Address::new(192, 168, 5, 33));
    assert_eq!(params.net_mask, Ipv4Address::new(255, 255, 255, 240));
    assert_eq!(
        params.gateway_mac,
        EthernetAddress([0x52, 0x54, 0, 168, 5, 33])
    );
    assert!(!params.advertise_routable_ipv6);
    assert!(params.map_gateway_to_host_loopback);

    let consomme = Consomme::new(params);
    for protocol in [IpProtocol::Tcp, IpProtocol::Udp] {
        assert_eq!(
            consomme
                .state
                .resolve_flow_destination(&"192.168.5.33:8080".parse().unwrap(), protocol),
            Some("127.0.0.1:8080".parse().unwrap())
        );
    }
}

#[test]
fn gateway_loopback_can_be_denied_with_one_proxy_exception() {
    let mut params = ConsommeParams::new().unwrap();
    params
        .set_static_ipv4(
            Ipv4Addr::new(192, 168, 5, 37),
            28,
            Ipv4Addr::new(192, 168, 5, 33),
            [0x52, 0x54, 0, 168, 5, 33],
        )
        .unwrap();
    params.map_gateway_to_host_loopback = false;
    params.gateway_loopback_proxy_port = Some(8443);
    let consomme = Consomme::new(params);

    for protocol in [IpProtocol::Tcp, IpProtocol::Udp] {
        for port in [8442, 8443, 8444] {
            let address = SocketAddr::from((Ipv4Addr::new(192, 168, 5, 33), port));
            let expected = (protocol == IpProtocol::Tcp && port == 8443)
                .then_some(SocketAddr::from((Ipv4Addr::LOCALHOST, 8443)));
            assert_eq!(
                consomme.state.resolve_flow_destination(&address, protocol),
                expected,
                "{protocol:?} port {port}"
            );
        }
    }
    assert_eq!(
        consomme
            .state
            .resolve_flow_destination(&"192.0.2.1:8443".parse().unwrap(), IpProtocol::Udp),
        Some("192.0.2.1:8443".parse().unwrap())
    );
}

#[pal_async::async_test]
async fn gateway_proxy_exception_rejects_udp(driver: DefaultDriver) {
    let listener = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let proxy_port = listener.local_addr().unwrap().port();
    let mut params = ConsommeParams::new().unwrap();
    params.map_gateway_to_host_loopback = false;
    params.gateway_loopback_proxy_port = Some(proxy_port);
    let guest_mac = params.client_mac;
    let gateway_mac = params.gateway_mac;
    let guest_ip = params.client_ip;
    let gateway_ip = params.gateway_ip;
    let mut consomme = Consomme::new(params);
    let mut client = TestClient::new(driver);

    for destination in [gateway_ip, Ipv4Addr::LOCALHOST] {
        let mut buf = [0u8; 1514];
        let len = build_ipv4_dns_query(
            &mut buf,
            guest_mac,
            gateway_mac,
            guest_ip,
            destination,
            44444,
            b"proxy-exception-must-be-tcp",
        );
        let mut ipv4 = Ipv4Packet::new_unchecked(&mut buf[ETHERNET_HEADER_LEN..len]);
        let mut udp = UdpPacket::new_unchecked(ipv4.payload_mut());
        udp.set_dst_port(proxy_port);
        udp.fill_checksum(&guest_ip.into(), &destination.into());
        let result = consomme
            .access(&mut client)
            .send(&buf[..len], &ChecksumState::NONE);
        assert!(
            matches!(result, Err(DropReason::DestinationNotAllowed)),
            "UDP proxy exception must be rejected, got {result:?}"
        );
    }
    assert_eq!(
        listener.recv(&mut [0; 64]).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn static_ipv4_identity_rejects_inconsistent_values() {
    let mut params = ConsommeParams::new().unwrap();
    assert!(
        params
            .set_static_ipv4(
                Ipv4Addr::new(10, 0, 0, 2),
                31,
                Ipv4Addr::new(10, 0, 0, 1),
                [0x52, 0x54, 0, 0, 0, 1],
            )
            .is_err()
    );
    assert!(
        params
            .set_static_ipv4(
                Ipv4Addr::new(10, 0, 0, 2),
                24,
                Ipv4Addr::new(10, 0, 0, 9),
                [0x52, 0x54, 0, 0, 0, 9],
            )
            .is_err()
    );
}
