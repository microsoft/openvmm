// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use crate::egress::EGRESS_POLICY_ENCODING_VERSION;
use crate::egress::EgressAction;
use crate::egress::EgressDestination;
use crate::egress::EgressRule;
use crate::egress::Ipv4Cidr;
use crate::egress::ParseEgressRuleError;
use std::net::Ipv4Addr;
use test_with_tracing::test;

const GUEST_IPV4: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 2);
const GATEWAY_IPV4: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);
const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0, 0, 0, 2];
const GATEWAY_MAC: [u8; 6] = [0x52, 0x54, 0, 0, 0, 1];
const GUEST_IPV6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0x0a00, 2);
const GATEWAY_IPV6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0x0a00, 1);
const PREFIX_LENGTH: u8 = 120;

fn address(value: &str) -> Ipv6Addr {
    value.parse().unwrap()
}

fn ipv4_policy(mode: EgressPolicyMode) -> EgressPolicy {
    EgressPolicy::bind(
        GUEST_IPV4,
        24,
        MacAddress::new(GUEST_MAC),
        GATEWAY_IPV4,
        mode,
    )
    .unwrap()
}

fn dual_stack(mode: EgressPolicyMode) -> EgressPolicy {
    ipv4_policy(mode)
        .with_ipv6(GUEST_IPV6, PREFIX_LENGTH, GATEWAY_IPV6)
        .unwrap()
}

fn rules(default_action: EgressAction, allow: &[&str], deny: &[&str]) -> EgressPolicyMode {
    let parse = |rules: &[&str]| {
        rules
            .iter()
            .map(|rule| rule.parse().unwrap())
            .collect::<Vec<EgressRule>>()
    };
    EgressPolicyMode::Rules {
        default_action,
        allow: parse(allow),
        deny: parse(deny),
    }
}

fn policy(default_action: EgressAction, allow: &[&str], deny: &[&str]) -> EgressPolicy {
    dual_stack(rules(default_action, allow, deny))
}

fn ipv6_frame(destination: Ipv6Addr, next_header: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0u8; 14 + 40];
    frame[..6].copy_from_slice(&GATEWAY_MAC);
    frame[6..12].copy_from_slice(&GUEST_MAC);
    frame[12..14].copy_from_slice(&ETHER_TYPE_IPV6.to_be_bytes());
    frame[14] = 0x60;
    frame[18..20].copy_from_slice(&(payload.len() as u16).to_be_bytes());
    frame[20] = next_header;
    frame[21] = 64;
    frame[22..38].copy_from_slice(&GUEST_IPV6.octets());
    frame[38..54].copy_from_slice(&destination.octets());
    frame.extend_from_slice(payload);
    frame
}

fn tcp(destination: Ipv6Addr, port: u16) -> Vec<u8> {
    let mut header = [0u8; 20];
    header[..2].copy_from_slice(&12345u16.to_be_bytes());
    header[2..4].copy_from_slice(&port.to_be_bytes());
    header[12] = 5 << 4;
    ipv6_frame(destination, NEXT_HEADER_TCP, &header)
}

fn udp(destination: Ipv6Addr, port: u16) -> Vec<u8> {
    let mut header = [0u8; 8];
    header[..2].copy_from_slice(&12345u16.to_be_bytes());
    header[2..4].copy_from_slice(&port.to_be_bytes());
    header[4..6].copy_from_slice(&8u16.to_be_bytes());
    ipv6_frame(destination, NEXT_HEADER_UDP, &header)
}

fn echo_request(destination: Ipv6Addr) -> Vec<u8> {
    let mut message = [0u8; 8];
    message[0] = 128;
    ipv6_frame(destination, NEXT_HEADER_ICMPV6, &message)
}

fn tcp_ipv4(destination: Ipv4Addr, port: u16) -> Vec<u8> {
    let mut frame = vec![0u8; 14 + 20 + 20];
    frame[6..12].copy_from_slice(&GUEST_MAC);
    frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
    frame[14] = 0x45;
    frame[16..18].copy_from_slice(&40u16.to_be_bytes());
    frame[22] = 64;
    frame[23] = 6;
    frame[26..30].copy_from_slice(&GUEST_IPV4.octets());
    frame[30..34].copy_from_slice(&destination.octets());
    frame[36..38].copy_from_slice(&port.to_be_bytes());
    frame[46] = 5 << 4;
    let checksum = !frame[14..34].chunks_exact(2).fold(0u32, |sum, word| {
        let sum = sum + u32::from(u16::from_be_bytes([word[0], word[1]]));
        (sum & 0xffff) + (sum >> 16)
    }) as u16;
    frame[24..26].copy_from_slice(&checksum.to_be_bytes());
    frame
}

fn gateway_arp_request() -> Vec<u8> {
    let mut frame = vec![0u8; 42];
    frame[..6].fill(0xff);
    frame[6..12].copy_from_slice(&GUEST_MAC);
    frame[12..14].copy_from_slice(&0x0806u16.to_be_bytes());
    frame[14..16].copy_from_slice(&1u16.to_be_bytes());
    frame[16..18].copy_from_slice(&0x0800u16.to_be_bytes());
    frame[18] = 6;
    frame[19] = 4;
    frame[20..22].copy_from_slice(&1u16.to_be_bytes());
    frame[22..28].copy_from_slice(&GUEST_MAC);
    frame[28..32].copy_from_slice(&GUEST_IPV4.octets());
    frame[38..42].copy_from_slice(&GATEWAY_IPV4.octets());
    frame
}

/// A Neighbor Solicitation of `target` from `source`, sent to the target's
/// solicited-node group or, without `multicast`, to the target itself.
fn solicitation(source: Ipv6Addr, target: Ipv6Addr, multicast: bool) -> Vec<u8> {
    let destination = if multicast {
        solicited_node_address(target)
    } else {
        target
    };
    let mut message = [0u8; 32];
    message[0] = NEIGHBOR_SOLICITATION;
    message[8..24].copy_from_slice(&target.octets());
    message[24] = SOURCE_LINK_LAYER_ADDRESS_OPTION;
    message[25] = 1;
    message[26..32].copy_from_slice(&GUEST_MAC);
    let mut frame = ipv6_frame(destination, NEXT_HEADER_ICMPV6, &message);
    frame[..6].copy_from_slice(&if multicast {
        multicast_mac_address(destination)
    } else {
        GATEWAY_MAC
    });
    frame[21] = NEIGHBOR_DISCOVERY_HOP_LIMIT;
    frame[22..38].copy_from_slice(&source.octets());
    frame
}

fn authorize(policy: &EgressPolicy, frame: &[u8]) -> Result<(), EgressDenied> {
    policy.authorize_frame(frame, frame.len())
}

#[test]
fn ipv6_networks_parse_canonically() {
    let network: Ipv6Cidr = "2001:db8:1::5/64".parse().unwrap();
    assert_eq!(network.network(), address("2001:db8:1::"));
    assert_eq!(network.prefix_length(), 64);
    assert!(network.contains(address("2001:db8:1::ffff")));
    assert!(!network.contains(address("2001:db8:2::")));
    let host: Ipv6Cidr = "2001:db8::1".parse().unwrap();
    assert_eq!(host.prefix_length(), 128);
    let everything: Ipv6Cidr = "::/0".parse().unwrap();
    assert!(everything.contains(address("ff02::1")));

    for (value, error) in [
        ("2001:db8::/129", ParseIpv6CidrError::PrefixOutOfRange(129)),
        (
            "2001:db8::/x",
            ParseIpv6CidrError::InvalidPrefix("x".to_owned()),
        ),
        (
            "2001:db8::zz",
            ParseIpv6CidrError::InvalidAddress("2001:db8::zz".to_owned()),
        ),
        (
            "fe80::1%eth0",
            ParseIpv6CidrError::InvalidAddress("fe80::1%eth0".to_owned()),
        ),
        (
            "2001:db8::/32/64",
            ParseIpv6CidrError::InvalidFormat("2001:db8::/32/64".to_owned()),
        ),
    ] {
        assert_eq!(value.parse::<Ipv6Cidr>(), Err(error), "{value}");
    }
}

#[test]
fn rules_name_either_family_with_the_same_selectors() {
    let v6 = |value: &str| EgressDestination::Ipv6(value.parse().unwrap());
    let v4 = |value: &str| EgressDestination::Ipv4(value.parse::<Ipv4Cidr>().unwrap());
    for (rule, destination, transport, port) in [
        (
            "2001:db8:1::/64:tcp:443",
            v6("2001:db8:1::/64"),
            EgressTransport::Tcp,
            443,
        ),
        (
            "2001:db8:1::/64",
            v6("2001:db8:1::/64"),
            EgressTransport::Any,
            0,
        ),
        (
            "2001:db8::1",
            v6("2001:db8::1/128"),
            EgressTransport::Any,
            0,
        ),
        (
            "2001:db8::1:tcp",
            v6("2001:db8::1/128"),
            EgressTransport::Tcp,
            0,
        ),
        (
            "2001:db8::1:2:udp:53",
            v6("2001:db8::1:2/128"),
            EgressTransport::Udp,
            53,
        ),
        (
            "2001:db8:::icmp",
            v6("2001:db8::/128"),
            EgressTransport::Icmp,
            0,
        ),
        ("::/0", v6("::/0"), EgressTransport::Any, 0),
        ("::/0:udp:53", v6("::/0"), EgressTransport::Udp, 53),
        ("::1:icmp", v6("::1/128"), EgressTransport::Icmp, 0),
        (
            "fd00::10.0.0.1:tcp:80",
            v6("fd00::a00:1/128"),
            EgressTransport::Tcp,
            80,
        ),
        ("0.0.0.0/0", v4("0.0.0.0/0"), EgressTransport::Any, 0),
        (
            "192.0.2.7:udp:53",
            v4("192.0.2.7"),
            EgressTransport::Udp,
            53,
        ),
    ] {
        let parsed: EgressRule = rule.parse().unwrap();
        assert_eq!(
            (parsed.destination(), parsed.transport(), parsed.port()),
            (destination, transport, port),
            "{rule}"
        );
    }

    for (rule, error) in [
        (
            "2001:db8::/64:sctp:443",
            ParseEgressRuleError::InvalidTransport("sctp".to_owned()),
        ),
        (
            "2001:db8::1:any",
            ParseEgressRuleError::InvalidTransport("any".to_owned()),
        ),
        ("2001:db8::/64:icmp:8", ParseEgressRuleError::IcmpPort),
        ("2001:db8::1:tcp:0", ParseEgressRuleError::ZeroPort),
        (
            "2001:db8::/64:tcp:443:extra",
            ParseEgressRuleError::InvalidFormat,
        ),
        (
            "2001:db8::/129:tcp:443",
            ParseEgressRuleError::InvalidIpv6Destination(ParseIpv6CidrError::PrefixOutOfRange(129)),
        ),
        (
            "2001:db8::zz",
            ParseEgressRuleError::InvalidIpv6Destination(ParseIpv6CidrError::InvalidAddress(
                "2001:db8::zz".to_owned(),
            )),
        ),
        (
            "192.0.2.1:sctp:443",
            ParseEgressRuleError::InvalidTransport("sctp".to_owned()),
        ),
    ] {
        assert_eq!(rule.parse::<EgressRule>(), Err(error), "{rule}");
    }
}

#[test]
fn the_ipv6_link_is_a_canonical_unicast_identity() {
    let bind = |guest: &str, prefix_length, gateway: &str| {
        ipv4_policy(EgressPolicyMode::AllowAll).with_ipv6(
            address(guest),
            prefix_length,
            address(gateway),
        )
    };
    let policy = bind("fd00::a00:2", 120, "fd00::a00:1").unwrap();
    let link = policy.ipv6_link().unwrap();
    assert_eq!(
        (link.guest(), link.prefix_length(), link.gateway()),
        (GUEST_IPV6, 120, GATEWAY_IPV6)
    );
    assert!(link.contains(address("fd00::a00:ff")));
    assert!(!link.contains(address("fd00::a01:0")));
    assert!(policy.validate().is_ok());

    for (guest, prefix_length, gateway, error) in [
        (
            "fd00::a00:2",
            0,
            "fd00::a00:1",
            InvalidEgressPolicy::Ipv6PrefixOutOfRange(0),
        ),
        (
            "fd00::a00:2",
            127,
            "fd00::a00:1",
            InvalidEgressPolicy::Ipv6PrefixOutOfRange(127),
        ),
        (
            "::",
            120,
            "::1",
            InvalidEgressPolicy::InvalidGuestIpv6(address("::")),
        ),
        (
            "::1",
            120,
            "::1",
            InvalidEgressPolicy::InvalidGuestIpv6(address("::1")),
        ),
        (
            "ff02::2",
            120,
            "ff02::1",
            InvalidEgressPolicy::InvalidGuestIpv6(address("ff02::2")),
        ),
        (
            "fe80::2",
            120,
            "fe80::1",
            InvalidEgressPolicy::InvalidGuestIpv6(address("fe80::2")),
        ),
        (
            "::ffff:10.0.0.2",
            120,
            "::ffff:10.0.0.1",
            InvalidEgressPolicy::InvalidGuestIpv6(address("::ffff:10.0.0.2")),
        ),
        (
            "fd00::a00:0",
            120,
            "fd00::a00:1",
            InvalidEgressPolicy::GuestIpv6NetworkAddress(address("fd00::a00:0")),
        ),
        (
            "fd00::a00:1",
            120,
            "fd00::a00:1",
            InvalidEgressPolicy::GuestIpv6GatewayCollision(address("fd00::a00:1")),
        ),
        (
            "fd00::a00:2",
            120,
            "fd00::a00:fe",
            InvalidEgressPolicy::Ipv6GatewayMismatch {
                actual: address("fd00::a00:fe"),
                expected: GATEWAY_IPV6,
            },
        ),
    ] {
        assert_eq!(
            bind(guest, prefix_length, gateway),
            Err(error),
            "{guest}/{prefix_length} via {gateway}"
        );
    }
}

/// The examples of microsoft/nvx#280: allow one network on one TCP port, deny
/// one address of it, and keep every other destination and port blocked.
#[test]
fn ipv6_rules_allow_one_network_and_port_with_deny_precedence() {
    let allowed = address("2001:db8:1::7");
    let denied = address("2001:db8:1::123");
    let outside = address("2001:db8:2::7");
    let policy = policy(
        EgressAction::Deny,
        &["2001:db8:1::/64:tcp:443"],
        &["2001:db8:1::123/128"],
    );
    assert!(policy.is_active());
    authorize(&policy, &tcp(allowed, 443)).unwrap();
    for frame in [
        tcp(allowed, 80),
        udp(allowed, 443),
        echo_request(allowed),
        tcp(denied, 443),
        tcp(outside, 443),
    ] {
        assert_eq!(
            authorize(&policy, &frame),
            Err(EgressDenied::DestinationDenied)
        );
    }
}

#[test]
fn ipv6_port_ranges_match_their_ports_with_deny_precedence() {
    let destination = address("2001:db8:1::7");
    let policy = policy(
        EgressAction::Deny,
        &["2001:db8:1::/64:tcp:8000-8010", "::/0:udp:5000-5001"],
        &["2001:db8:1::7:tcp:8005"],
    );
    for frame in [
        tcp(destination, 8000),
        tcp(destination, 8004),
        tcp(destination, 8010),
        udp(address("2001:db8:9::1"), 5001),
    ] {
        authorize(&policy, &frame).unwrap();
    }
    for frame in [
        tcp(destination, 7999),
        tcp(destination, 8005),
        tcp(destination, 8011),
        udp(destination, 8000),
        udp(destination, 5002),
        tcp_ipv4(Ipv4Addr::new(192, 0, 2, 7), 8000),
    ] {
        assert!(authorize(&policy, &frame).is_err());
    }

    // The tagged encoding tells a range apart from its first port.
    let single = policy_bytes(&["2001:db8:1::/64:tcp:8000"]);
    let range = policy_bytes(&["2001:db8:1::/64:tcp:8000-8010"]);
    assert_ne!(single, range);
    assert_eq!(single, policy_bytes(&["2001:db8:1::/64:tcp:8000-8000"]));
}

fn policy_bytes(allow: &[&str]) -> Vec<u8> {
    policy(EgressAction::Deny, allow, &[]).canonical_bytes()
}

#[test]
fn denying_egress_without_allow_rules_blocks_ipv6_too() {
    let destination = address("2001:db8::7");
    for policy in [
        policy(EgressAction::Deny, &[], &["192.0.2.0/24"]),
        policy(EgressAction::Deny, &[], &["2001:db8::/32:tcp:443"]),
        dual_stack(EgressPolicyMode::DenyAll),
    ] {
        assert!(policy.is_active());
        for frame in [
            tcp(destination, 443),
            udp(destination, 53),
            echo_request(GATEWAY_IPV6),
        ] {
            assert!(authorize(&policy, &frame).is_err(), "{policy:?}");
        }
        // Without an IPv6 rule, the guest cannot even resolve the gateway.
        assert!(
            authorize(&policy, &solicitation(GUEST_IPV6, GATEWAY_IPV6, true)).is_err(),
            "{policy:?}"
        );
    }
}

#[test]
fn rules_of_one_family_never_match_the_other() {
    let ipv6 = address("2001:db8::7");
    let ipv4 = Ipv4Addr::new(192, 0, 2, 7);

    let ipv4_only = policy(EgressAction::Deny, &["0.0.0.0/0"], &[]);
    authorize(&ipv4_only, &tcp_ipv4(ipv4, 443)).unwrap();
    assert_eq!(
        authorize(&ipv4_only, &tcp(ipv6, 443)),
        Err(EgressDenied::DestinationDenied)
    );
    assert_eq!(
        authorize(&ipv4_only, &solicitation(GUEST_IPV6, GATEWAY_IPV6, true)),
        Err(EgressDenied::NeighborDiscoveryDenied)
    );

    let ipv6_only = policy(EgressAction::Deny, &["::/0"], &[]);
    authorize(&ipv6_only, &tcp(ipv6, 443)).unwrap();
    authorize(&ipv6_only, &solicitation(GUEST_IPV6, GATEWAY_IPV6, true)).unwrap();
    assert_eq!(
        authorize(&ipv6_only, &tcp_ipv4(ipv4, 443)),
        Err(EgressDenied::DestinationDenied)
    );
    // The IPv4 gateway is not a next hop of a policy that admits only IPv6.
    assert!(ipv6_only.next_hops().is_empty());
    assert_eq!(
        authorize(&ipv6_only, &gateway_arp_request()),
        Err(EgressDenied::ArpDenied)
    );
    authorize(&ipv4_only, &gateway_arp_request()).unwrap();

    // Denying an IPv4 network under an allow default leaves IPv6 open, and
    // denying an IPv6 network leaves IPv4 open.
    let ipv4_denied = policy(EgressAction::Allow, &[], &["0.0.0.0/0"]);
    authorize(&ipv4_denied, &tcp(ipv6, 443)).unwrap();
    assert_eq!(
        authorize(&ipv4_denied, &tcp_ipv4(ipv4, 443)),
        Err(EgressDenied::DestinationDenied)
    );
    let ipv6_denied = policy(EgressAction::Allow, &[], &["::/0"]);
    authorize(&ipv6_denied, &tcp_ipv4(ipv4, 443)).unwrap();
    assert_eq!(
        authorize(&ipv6_denied, &tcp(ipv6, 443)),
        Err(EgressDenied::DestinationDenied)
    );
}

#[test]
fn ipv4_mapped_destinations_cannot_bypass_ipv4_rules() {
    let policy = policy(EgressAction::Deny, &["::/0"], &["192.0.2.0/24"]);
    for mapped in ["::ffff:192.0.2.7", "::ffff:127.0.0.1", "::ffff:10.0.0.1"] {
        assert_eq!(
            authorize(&policy, &tcp(address(mapped), 443)),
            Err(EgressDenied::Ipv4MappedDestinationDenied),
            "{mapped}"
        );
    }
}

#[test]
fn the_wildcard_matches_every_ipv6_destination_and_only_ipv6() {
    let policy = policy(EgressAction::Deny, &["::/0:tcp:443", "::/0:icmp"], &[]);
    for destination in ["2001:db8::1", "2606:4700::1111", "fd00::a00:1"] {
        let destination = address(destination);
        authorize(&policy, &tcp(destination, 443)).unwrap();
        authorize(&policy, &echo_request(destination)).unwrap();
        assert_eq!(
            authorize(&policy, &udp(destination, 443)),
            Err(EgressDenied::DestinationDenied)
        );
    }
    assert_eq!(
        authorize(&policy, &tcp_ipv4(Ipv4Addr::new(192, 0, 2, 7), 443)),
        Err(EgressDenied::DestinationDenied)
    );
}

#[test]
fn an_allow_default_without_deny_rules_leaves_ipv6_unfiltered() {
    let policy = policy(EgressAction::Allow, &["::/0:tcp:443"], &[]);
    assert!(!policy.is_active());
    authorize(&policy, &udp(address("2001:db8::7"), 53)).unwrap();
}

#[test]
fn ipv6_needs_a_dual_stack_link_and_rules() {
    let destination = address("2001:db8::7");
    let unbound = ipv4_policy(rules(EgressAction::Deny, &["::/0"], &[]));
    assert!(unbound.ipv6_link().is_none());
    assert_eq!(
        authorize(&unbound, &tcp(destination, 443)),
        Err(EgressDenied::UnsupportedEtherType(ETHER_TYPE_IPV6))
    );
    for mode in [
        EgressPolicyMode::AllowList(vec!["0.0.0.0/0".parse().unwrap()]),
        EgressPolicyMode::BlockList(vec!["192.0.2.0/24".parse().unwrap()]),
        EgressPolicyMode::TcpEndpoints(vec!["192.0.2.7:443".parse().unwrap()]),
    ] {
        let policy = dual_stack(mode);
        for frame in [
            tcp(destination, 443),
            solicitation(GUEST_IPV6, GATEWAY_IPV6, true),
        ] {
            assert_eq!(
                authorize(&policy, &frame),
                Err(EgressDenied::UnsupportedEtherType(ETHER_TYPE_IPV6)),
                "{policy:?}"
            );
        }
    }
}

#[test]
fn malformed_spoofed_and_extended_ipv6_packets_are_denied() {
    let destination = address("2001:db8::7");
    let policy = policy(EgressAction::Deny, &["::/0"], &[]);
    let allowed = tcp(destination, 443);
    authorize(&policy, &allowed).unwrap();

    let mut spoofed = allowed.clone();
    spoofed[37] ^= 1;
    assert_eq!(
        authorize(&policy, &spoofed),
        Err(EgressDenied::SourceAddressDenied)
    );
    let mut link_local_source = allowed.clone();
    link_local_source[22..38]
        .copy_from_slice(&link_local_address(MacAddress::new(GUEST_MAC)).octets());
    assert_eq!(
        authorize(&policy, &link_local_source),
        Err(EgressDenied::SourceAddressDenied)
    );
    let mut spoofed_mac = allowed.clone();
    spoofed_mac[11] ^= 1;
    assert_eq!(
        authorize(&policy, &spoofed_mac),
        Err(EgressDenied::SourceMacDenied)
    );

    // Hop-by-hop options, routing, fragment, and destination options headers.
    for next_header in [0, 43, 44, 60] {
        let extended = ipv6_frame(
            destination,
            next_header,
            &[NEXT_HEADER_TCP, 0, 0, 0, 0, 0, 0, 0],
        );
        assert_eq!(
            authorize(&policy, &extended),
            Err(EgressDenied::Ipv6ExtensionHeaderDenied),
            "{next_header}"
        );
    }

    let mut version = allowed.clone();
    version[14] = 0x40;
    assert_eq!(
        authorize(&policy, &version),
        Err(EgressDenied::Malformed("IPv6 header"))
    );
    assert_eq!(
        authorize(&policy, &allowed[..50]),
        Err(EgressDenied::Malformed("IPv6 header"))
    );
    let mut long_payload = allowed.clone();
    long_payload[18..20].copy_from_slice(&21u16.to_be_bytes());
    assert_eq!(
        authorize(&policy, &long_payload),
        Err(EgressDenied::Malformed("IPv6 payload length"))
    );
    let mut long_header = allowed.clone();
    long_header[66] = 6 << 4;
    assert_eq!(
        authorize(&policy, &long_header),
        Err(EgressDenied::Malformed("TCP header"))
    );
    let mut short_udp = udp(destination, 53);
    short_udp[58..60].copy_from_slice(&7u16.to_be_bytes());
    assert_eq!(
        authorize(&policy, &short_udp),
        Err(EgressDenied::Malformed("UDP length"))
    );
    let mut short_icmp = ipv6_frame(destination, NEXT_HEADER_ICMPV6, &[128, 0, 0, 0]);
    assert_eq!(
        authorize(&policy, &short_icmp),
        Err(EgressDenied::Malformed("ICMPv6 header"))
    );
    short_icmp.extend_from_slice(&[0; 4]);
    assert_eq!(
        authorize(&policy, &short_icmp),
        Err(EgressDenied::Malformed("ICMPv6 length"))
    );

    // Trailing Ethernet padding beyond the IPv6 payload is permitted.
    let mut padded = allowed;
    padded.extend_from_slice(&[0; 6]);
    authorize(&policy, &padded).unwrap();
}

#[test]
fn vlan_tagged_ipv6_is_authorized_by_its_inner_header() {
    let policy = policy(EgressAction::Deny, &["2001:db8::/32:tcp:443"], &[]);
    let tag = |frame: Vec<u8>| {
        let mut tagged = frame[..12].to_vec();
        tagged.extend_from_slice(&0x8100u16.to_be_bytes());
        tagged.extend_from_slice(&[0, 7]);
        tagged.extend_from_slice(&frame[12..]);
        tagged
    };
    authorize(&policy, &tag(tcp(address("2001:db8::7"), 443))).unwrap();
    assert_eq!(
        authorize(&policy, &tag(tcp(address("2001:db8::7"), 80))),
        Err(EgressDenied::DestinationDenied)
    );
}

#[test]
fn neighbor_discovery_resolves_only_authorized_on_link_neighbors() {
    let link_local = link_local_address(MacAddress::new(GUEST_MAC));
    let neighbor = address("fd00::a00:9");
    let policy = policy(EgressAction::Deny, &["2001:db8::/32:tcp:443"], &[]);

    // The gateway, through its solicited-node group or directly to confirm
    // reachability, from either guest address.
    for (source, multicast) in [(GUEST_IPV6, true), (GUEST_IPV6, false), (link_local, true)] {
        authorize(&policy, &solicitation(source, GATEWAY_IPV6, multicast)).unwrap();
    }

    // Neighbors that no rule admits, the guest itself, off-link targets, and
    // solicitations from other sources or for duplicate address detection.
    for frame in [
        solicitation(GUEST_IPV6, neighbor, true),
        solicitation(GUEST_IPV6, GUEST_IPV6, true),
        solicitation(GUEST_IPV6, address("2001:db8::7"), true),
        solicitation(address("fd00::a00:3"), GATEWAY_IPV6, true),
        solicitation(Ipv6Addr::UNSPECIFIED, GATEWAY_IPV6, true),
    ] {
        assert_eq!(
            authorize(&policy, &frame),
            Err(EgressDenied::NeighborDiscoveryDenied)
        );
    }

    // Other Neighbor Discovery messages.
    for message in [133, 134, 136, 137] {
        let mut frame = solicitation(GUEST_IPV6, GATEWAY_IPV6, true);
        frame[54] = message;
        assert_eq!(
            authorize(&policy, &frame),
            Err(EgressDenied::NeighborDiscoveryDenied),
            "{message}"
        );
    }

    // Misaddressed solicitations and foreign link-layer addresses.
    let mut multicast_to_unicast = solicitation(GUEST_IPV6, GATEWAY_IPV6, true);
    multicast_to_unicast[..6].copy_from_slice(&GATEWAY_MAC);
    let mut wrong_group = solicitation(GUEST_IPV6, GATEWAY_IPV6, true);
    wrong_group[53] ^= 1;
    let mut foreign_option = solicitation(GUEST_IPV6, GATEWAY_IPV6, true);
    foreign_option[85] ^= 1;
    for frame in [multicast_to_unicast, wrong_group, foreign_option] {
        assert_eq!(
            authorize(&policy, &frame),
            Err(EgressDenied::NeighborDiscoveryDenied)
        );
    }

    let mut forwarded = solicitation(GUEST_IPV6, GATEWAY_IPV6, true);
    forwarded[21] = 64;
    let mut empty_option = solicitation(GUEST_IPV6, GATEWAY_IPV6, true);
    empty_option[79] = 0;
    for (frame, reason) in [
        (forwarded, "Neighbor Solicitation"),
        (empty_option, "Neighbor Discovery option"),
    ] {
        assert_eq!(
            authorize(&policy, &frame),
            Err(EgressDenied::Malformed(reason))
        );
    }

    // An on-link neighbor follows the rules, like an IPv4 ARP target.
    let allowed = dual_stack(rules(EgressAction::Deny, &["fd00::a00:9"], &[]));
    authorize(&allowed, &solicitation(GUEST_IPV6, neighbor, true)).unwrap();
    let denied = dual_stack(rules(EgressAction::Allow, &[], &["fd00::a00:9"]));
    assert_eq!(
        authorize(&denied, &solicitation(GUEST_IPV6, neighbor, true)),
        Err(EgressDenied::NeighborDiscoveryDenied)
    );
    let protocol_denied = dual_stack(rules(EgressAction::Allow, &[], &["fd00::a00:9:tcp"]));
    authorize(&protocol_denied, &solicitation(GUEST_IPV6, neighbor, true)).unwrap();
}

#[test]
fn ipv6_port_rules_leave_ipv4_fragments_to_ipv4_rules() {
    let policy = policy(
        EgressAction::Deny,
        &["192.0.2.7:tcp", "2001:db8::/32:tcp:443"],
        &[],
    );
    let mut fragment = tcp_ipv4(Ipv4Addr::new(192, 0, 2, 7), 443);
    fragment[20..22].copy_from_slice(&0x00b9u16.to_be_bytes());
    let checksum_offset = 24;
    fragment[checksum_offset..checksum_offset + 2].fill(0);
    let checksum = !fragment[14..34].chunks_exact(2).fold(0u32, |sum, word| {
        let sum = sum + u32::from(u16::from_be_bytes([word[0], word[1]]));
        (sum & 0xffff) + (sum >> 16)
    }) as u16;
    fragment[checksum_offset..checksum_offset + 2].copy_from_slice(&checksum.to_be_bytes());
    authorize(&policy, &fragment).unwrap();
}

#[test]
fn gateway_dns_prefers_ipv4_and_falls_back_to_ipv6() {
    let ipv4 = Some(IpAddr::V4(GATEWAY_IPV4));
    let ipv6 = Some(IpAddr::V6(GATEWAY_IPV6));
    let cases: [(EgressAction, &[&str], &[&str], Option<IpAddr>); 11] = [
        (EgressAction::Allow, &[], &[], ipv4),
        (
            EgressAction::Deny,
            &["10.0.0.1:udp:53", "fd00::a00:1:udp:53"],
            &[],
            ipv4,
        ),
        (EgressAction::Deny, &["fd00::a00:1:udp:53"], &[], ipv6),
        (EgressAction::Deny, &["fd00::a00:1/128:tcp"], &[], ipv6),
        (EgressAction::Deny, &["::/0"], &[], ipv6),
        (EgressAction::Allow, &[], &["10.0.0.1"], ipv6),
        (
            EgressAction::Deny,
            &["::/0"],
            &["fd00::a00:1:tcp:53", "fd00::a00:1:udp:53"],
            None,
        ),
        (EgressAction::Deny, &["fd00::a00:1:icmp"], &[], None),
        (EgressAction::Deny, &["fd00::a00:1:udp:54"], &[], None),
        // An IPv4 rule never admits the IPv6 gateway.
        (EgressAction::Deny, &["0.0.0.0/0"], &["10.0.0.1"], None),
        (EgressAction::Allow, &[], &["10.0.0.1", "fd00::/8"], None),
    ];
    for (default_action, allow, deny, expected) in cases {
        assert_eq!(
            policy(default_action, allow, deny).gateway_dns_server(),
            expected,
            "{default_action:?} {allow:?} {deny:?}"
        );
    }
    // Without an IPv6 identity, and in the legacy modes, which deny all IPv6,
    // only the IPv4 gateway can serve DNS.
    let unbound = ipv4_policy(rules(EgressAction::Deny, &["fd00::a00:1:udp:53"], &[]));
    assert_eq!(unbound.gateway_dns_server(), None);
    for (mode, expected) in [
        (
            EgressPolicyMode::AllowList(vec!["10.0.0.1/32".parse().unwrap()]),
            ipv4,
        ),
        (
            EgressPolicyMode::BlockList(vec!["10.0.0.0/24".parse().unwrap()]),
            None,
        ),
        (EgressPolicyMode::DenyAll, None),
        (EgressPolicyMode::AllowAll, ipv4),
    ] {
        assert_eq!(dual_stack(mode).gateway_dns_server(), expected);
    }
}

#[test]
fn ipv6_policies_have_their_own_canonical_encoding() {
    let ipv4 = ipv4_policy(rules(EgressAction::Deny, &["192.0.2.0/24:tcp:443"], &[]));
    assert_eq!(ipv4.encoding_version(), 3);
    assert_eq!(
        ipv4.canonical_bytes_for_version(3),
        Some(ipv4.canonical_bytes())
    );
    assert_eq!(ipv4.canonical_bytes()[0], 3);
    assert!(ipv4.canonical_bytes_for_version(4).is_some());

    let dual = dual_stack(rules(EgressAction::Deny, &["192.0.2.0/24:tcp:443"], &[]));
    let with_rule = dual_stack(rules(
        EgressAction::Deny,
        &["192.0.2.0/24:tcp:443", "2001:db8::/32:tcp:443"],
        &[],
    ));
    let unbound_rule = ipv4_policy(rules(EgressAction::Deny, &["2001:db8::/32:tcp:443"], &[]));
    for policy in [&dual, &with_rule, &unbound_rule] {
        assert_eq!(policy.encoding_version(), EGRESS_POLICY_ENCODING_VERSION);
        assert_eq!(
            policy.canonical_bytes_for_version(EGRESS_POLICY_ENCODING_VERSION),
            Some(policy.canonical_bytes())
        );
        for version in 1..EGRESS_POLICY_ENCODING_VERSION {
            assert_eq!(policy.canonical_bytes_for_version(version), None);
        }
        assert!(policy.validate().is_ok());
    }
    let encodings = [
        ipv4.canonical_bytes_for_version(4).unwrap(),
        dual.canonical_bytes(),
        with_rule.canonical_bytes(),
        unbound_rule.canonical_bytes(),
    ];
    for (index, encoding) in encodings.iter().enumerate() {
        assert!(!encodings[index + 1..].contains(encoding));
    }

    // A rule's family tag distinguishes an IPv6 network from an IPv4 one.
    let record = [
        6, 0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 32, 1, 1, 187,
    ];
    assert!(
        with_rule
            .canonical_bytes()
            .windows(record.len())
            .any(|window| window == record)
    );
}
