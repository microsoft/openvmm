// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of the exact static IPv6 identity of a dual-stack link.

use super::*;
use smoltcp::wire::Icmpv6Repr;
use smoltcp::wire::IpAddress;
use smoltcp::wire::NdiscRepr;
use smoltcp::wire::RawHardwareAddress;
use std::net::Ipv6Addr;

const GUEST_MAC: EthernetAddress = EthernetAddress([0x52, 0x54, 0, 0, 0, 2]);
const GATEWAY_MAC: [u8; 6] = [0x52, 0x54, 0, 0, 0, 1];
const GUEST_IPV6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0x0a00, 2);
const GATEWAY_IPV6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0x0a00, 1);

fn dual_stack_params() -> ConsommeParams {
    let mut params = ConsommeParams::new().unwrap();
    params.client_mac = GUEST_MAC;
    params
        .set_static_ipv4(
            Ipv4Addr::new(10, 0, 0, 2),
            24,
            Ipv4Addr::new(10, 0, 0, 1),
            GATEWAY_MAC,
        )
        .unwrap();
    params
        .set_static_ipv6(GUEST_IPV6, 120, GATEWAY_IPV6)
        .unwrap();
    params
}

/// Builds an Ethernet/IPv6/ICMPv6 frame from the guest that carries `icmp`.
fn icmpv6_frame(
    destination: Ipv6Addr,
    eth_destination: EthernetAddress,
    icmp: &Icmpv6Repr<'_>,
) -> Vec<u8> {
    let source = GUEST_IPV6;
    let mut buf =
        vec![0u8; ETHERNET_HEADER_LEN + smoltcp::wire::IPV6_HEADER_LEN + icmp.buffer_len()];
    let mut eth = EthernetFrame::new_unchecked(&mut buf[..]);
    eth.set_src_addr(GUEST_MAC);
    eth.set_dst_addr(eth_destination);
    eth.set_ethertype(EthernetProtocol::Ipv6);
    let ip_repr = Ipv6Repr {
        src_addr: source,
        dst_addr: destination,
        next_header: IpProtocol::Icmpv6,
        payload_len: icmp.buffer_len(),
        hop_limit: 255,
    };
    let mut ipv6 = Ipv6Packet::new_unchecked(eth.payload_mut());
    ip_repr.emit(&mut ipv6);
    icmp.emit(
        &source,
        &destination,
        &mut Icmpv6Packet::new_unchecked(ipv6.payload_mut()),
        &ChecksumCapabilities::default(),
    );
    buf
}

fn solicitation(target: Ipv6Addr) -> Vec<u8> {
    let octets = target.octets();
    let group = Ipv6Addr::from([
        0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0xff, octets[13], octets[14], octets[15],
    ]);
    icmpv6_frame(
        group,
        EthernetAddress([0x33, 0x33, 0xff, octets[13], octets[14], octets[15]]),
        &Icmpv6Repr::Ndisc(NdiscRepr::NeighborSolicit {
            target_addr: target,
            lladdr: Some(RawHardwareAddress::from(GUEST_MAC)),
        }),
    )
}

#[test]
fn static_ipv6_identity_shares_the_gateway_mac_and_maps_the_gateway_to_ipv6_loopback() {
    let params = dual_stack_params();
    assert_eq!(params.client_ip_ipv6_routable, Some(GUEST_IPV6));
    assert_eq!(
        params.client_ip_ipv6,
        Some(ConsommeParams::compute_link_local_address(GUEST_MAC))
    );
    assert_eq!(params.gateway_ipv6, Some(GATEWAY_IPV6));
    assert_eq!(params.prefix_len_ipv6, 120);
    assert_eq!(params.gateway_mac_ipv6, EthernetAddress(GATEWAY_MAC));
    assert_eq!(
        params.gateway_link_local_ipv6,
        ConsommeParams::compute_link_local_address(EthernetAddress(GATEWAY_MAC))
    );
    assert!(!params.advertise_routable_ipv6);
    assert!(params.skip_ipv6_checks);

    let mut consomme = Consomme::new(params);
    assert!(consomme.host_has_ipv6);
    for protocol in [IpProtocol::Tcp, IpProtocol::Udp] {
        assert_eq!(
            consomme
                .state
                .resolve_flow_destination(&"[fd00::a00:1]:8080".parse().unwrap(), protocol),
            Some("[::1]:8080".parse().unwrap())
        );
        assert_eq!(
            consomme
                .state
                .resolve_flow_destination(&"[2001:db8::1]:8080".parse().unwrap(), protocol),
            Some("[2001:db8::1]:8080".parse().unwrap())
        );
    }

    // The proxy port is an IPv4 exception only.
    consomme.params_mut().map_gateway_to_host_loopback = false;
    consomme.params_mut().gateway_loopback_proxy_port = Some(8080);
    assert_eq!(
        consomme
            .state
            .resolve_flow_destination(&"[fd00::a00:1]:8080".parse().unwrap(), IpProtocol::Tcp),
        None
    );
}

#[test]
fn static_ipv6_identity_rejects_inconsistent_values() {
    let mut params = ConsommeParams::new().unwrap();
    for (guest, prefix_length, gateway) in [
        ("fd00::a00:2", 0, "fd00::a00:1"),
        ("fd00::a00:2", 127, "fd00::a00:1"),
        ("fd00::a00:2", 120, "fd00::a00:9"),
        ("fd00::a00:1", 120, "fd00::a00:1"),
        ("fd00::a00:0", 120, "fd00::a00:1"),
        ("fe80::2", 120, "fe80::1"),
        ("::ffff:10.0.0.2", 120, "::ffff:10.0.0.1"),
        ("ff02::2", 120, "ff02::1"),
        ("::", 120, "::1"),
    ] {
        assert!(
            params
                .set_static_ipv6(
                    guest.parse().unwrap(),
                    prefix_length,
                    gateway.parse().unwrap()
                )
                .is_err(),
            "{guest}/{prefix_length} via {gateway}"
        );
    }
    assert_eq!(params.gateway_ipv6, None);
}

#[pal_async::async_test]
async fn the_gateway_answers_neighbor_solicitations_for_its_subnet_only(driver: DefaultDriver) {
    let mut consomme = Consomme::new(dual_stack_params());
    let mut client = CapturingClient::new(driver);
    for target in [
        GATEWAY_IPV6,
        "fd00::a00:9".parse().unwrap(),
        GUEST_IPV6,
        "2001:db8::1".parse().unwrap(),
    ] {
        consomme
            .access(&mut client)
            .send(&solicitation(target), &ChecksumState::NONE)
            .unwrap();
    }
    // The gateway advertises itself for its address and its other on-link
    // neighbors, never for the guest or an off-link address.
    let targets = client
        .received
        .iter()
        .map(|frame| {
            let eth = EthernetFrame::new_checked(&frame[..]).unwrap();
            assert_eq!(eth.src_addr(), EthernetAddress(GATEWAY_MAC));
            assert_eq!(eth.dst_addr(), GUEST_MAC);
            let ipv6 = Ipv6Packet::new_checked(eth.payload()).unwrap();
            assert_eq!(ipv6.dst_addr(), GUEST_IPV6);
            let icmp = Icmpv6Packet::new_checked(ipv6.payload()).unwrap();
            assert!(icmp.verify_checksum(&ipv6.src_addr(), &ipv6.dst_addr()));
            match Icmpv6Repr::parse(
                &ipv6.src_addr(),
                &ipv6.dst_addr(),
                &icmp,
                &ChecksumCapabilities::default(),
            )
            .unwrap()
            {
                Icmpv6Repr::Ndisc(NdiscRepr::NeighborAdvert {
                    target_addr,
                    lladdr: Some(lladdr),
                    ..
                }) => {
                    assert_eq!(
                        lladdr,
                        RawHardwareAddress::from(EthernetAddress(GATEWAY_MAC))
                    );
                    target_addr
                }
                other => panic!("unexpected reply {other:?}"),
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        targets,
        [GATEWAY_IPV6, "fd00::a00:9".parse::<Ipv6Addr>().unwrap()]
    );
}

#[pal_async::async_test]
async fn the_gateway_answers_echo_requests_itself(driver: DefaultDriver) {
    let mut consomme = Consomme::new(dual_stack_params());
    let mut client = CapturingClient::new(driver);
    let request = icmpv6_frame(
        GATEWAY_IPV6,
        EthernetAddress(GATEWAY_MAC),
        &Icmpv6Repr::EchoRequest {
            ident: 0x1234,
            seq_no: 7,
            data: b"nvx-ipv6-gateway-echo",
        },
    );
    consomme
        .access(&mut client)
        .send(&request, &ChecksumState::NONE)
        .unwrap();
    let [reply] = client.received.as_slice() else {
        panic!("expected one echo reply, got {}", client.received.len());
    };
    let eth = EthernetFrame::new_checked(&reply[..]).unwrap();
    assert_eq!(eth.dst_addr(), GUEST_MAC);
    let ipv6 = Ipv6Packet::new_checked(eth.payload()).unwrap();
    assert_eq!(
        (ipv6.src_addr(), ipv6.dst_addr()),
        (GATEWAY_IPV6, GUEST_IPV6)
    );
    let icmp = Icmpv6Packet::new_checked(ipv6.payload()).unwrap();
    assert_eq!(
        Icmpv6Repr::parse(
            &ipv6.src_addr(),
            &ipv6.dst_addr(),
            &icmp,
            &ChecksumCapabilities::default(),
        )
        .unwrap(),
        Icmpv6Repr::EchoReply {
            ident: 0x1234,
            seq_no: 7,
            data: b"nvx-ipv6-gateway-echo",
        }
    );

    // Echo requests to other destinations are not relayed.
    let other = icmpv6_frame(
        "2001:db8::1".parse().unwrap(),
        EthernetAddress(GATEWAY_MAC),
        &Icmpv6Repr::EchoRequest {
            ident: 1,
            seq_no: 1,
            data: &[],
        },
    );
    assert!(matches!(
        consomme
            .access(&mut client)
            .send(&other, &ChecksumState::NONE),
        Err(DropReason::UnsupportedIcmpv6(Icmpv6Message::EchoRequest))
    ));
}

#[pal_async::async_test]
async fn ipv4_mapped_destinations_are_rejected_even_with_host_local_access(driver: DefaultDriver) {
    let mut params = dual_stack_params();
    params.allow_host_local_access = true;
    let mut consomme = Consomme::new(params);
    let mut client = TestClient::new(driver);
    for destination in ["::ffff:127.0.0.1", "::ffff:192.0.2.7", "::ffff:10.0.0.1"] {
        let mut buf = vec![0u8; 1514];
        let len = build_ipv6_syn(
            &mut buf,
            GUEST_MAC,
            EthernetAddress(GATEWAY_MAC),
            GUEST_IPV6,
            destination.parse().unwrap(),
        );
        let result = consomme
            .access(&mut client)
            .send(&buf[..len], &ChecksumState::NONE);
        assert!(
            matches!(result, Err(DropReason::DestinationNotAllowed)),
            "{destination}: {result:?}"
        );
    }
}

#[pal_async::async_test]
async fn truncated_icmpv6_is_rejected_without_panicking(driver: DefaultDriver) {
    let mut consomme = Consomme::new(dual_stack_params());
    let mut client = TestClient::new(driver);
    for length in [0, 1, 3] {
        let mut frame = icmpv6_frame(
            GATEWAY_IPV6,
            EthernetAddress(GATEWAY_MAC),
            &Icmpv6Repr::EchoRequest {
                ident: 1,
                seq_no: 1,
                data: &[],
            },
        );
        frame.truncate(ETHERNET_HEADER_LEN + smoltcp::wire::IPV6_HEADER_LEN + length);
        let mut ipv6 = Ipv6Packet::new_unchecked(&mut frame[ETHERNET_HEADER_LEN..]);
        ipv6.set_payload_len(length as u16);
        let result = consomme
            .access(&mut client)
            .send(&frame, &ChecksumState::NONE);
        assert!(
            matches!(result, Err(DropReason::Packet(_))),
            "{length}: {result:?}"
        );
    }
}

/// The guest's TCP connection to the IPv6 gateway reaches a host service on
/// IPv6 loopback, and is refused when host loopback is denied.
#[pal_async::async_test]
async fn gateway_tcp_reaches_host_ipv6_loopback(driver: DefaultDriver) {
    let listener = match std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)) {
        Ok(listener) => listener,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
            ) =>
        {
            return;
        }
        Err(err) => panic!("IPv6 loopback bind failed: {err}"),
    };
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let syn = |buf: &mut [u8]| {
        let len = build_ipv6_syn(
            buf,
            GUEST_MAC,
            EthernetAddress(GATEWAY_MAC),
            GUEST_IPV6,
            GATEWAY_IPV6,
        );
        let mut ipv6 = Ipv6Packet::new_unchecked(&mut buf[ETHERNET_HEADER_LEN..len]);
        let mut tcp = TcpPacket::new_unchecked(ipv6.payload_mut());
        tcp.set_dst_port(port);
        tcp.fill_checksum(&IpAddress::Ipv6(GUEST_IPV6), &IpAddress::Ipv6(GATEWAY_IPV6));
        len
    };

    let mut denied = dual_stack_params();
    denied.map_gateway_to_host_loopback = false;
    let mut consomme = Consomme::new(denied);
    let mut client = TestClient::new(driver.clone());
    let mut buf = vec![0u8; 1514];
    let len = syn(&mut buf);
    assert!(matches!(
        consomme
            .access(&mut client)
            .send(&buf[..len], &ChecksumState::NONE),
        Err(DropReason::DestinationNotAllowed)
    ));

    let mut consomme = Consomme::new(dual_stack_params());
    let mut client = TestClient::new(driver);
    consomme
        .access(&mut client)
        .send(&buf[..len], &ChecksumState::NONE)
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match listener.accept() {
            Ok((_, peer)) => {
                assert!(peer.ip().is_loopback(), "{peer}");
                break;
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the gateway connection did not reach host IPv6 loopback"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => panic!("accept failed: {err}"),
        }
    }
}
