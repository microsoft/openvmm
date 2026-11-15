// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of the echo relay: how replies find their requests, and the relay
//! through a host socket.

use super::*;
use crate::Consomme;
use crate::ConsommeParams;
use crate::icmp::echo::ECHO_TIMEOUT;
use crate::icmp::echo::MAX_PENDING_ECHOES;
use crate::icmp::echo::Reply;
use crate::icmp::echo::split_delivery;
use crate::tests::DriverThread;
use crate::tests::TestClient;
use crate::tests::WakeChannel;
use test_with_tracing::test;

/// The identifier that the guest gives its echo requests.
pub(super) const GUEST_IDENT: u16 = 0x1234;
/// The identifier that a datagram socket of the host gives them instead.
const SOCKET_IDENT: u16 = 0x7777;
/// The identifier that a flow's requests carry through a raw socket.
pub(super) const FLOW_IDENT: u16 = 0x5a5a;
/// The host that the guest pings.
const HOST: Ipv4Addr = Ipv4Addr::new(198, 51, 100, 7);

/// Returns an empty table of the flow that the tests use.
pub(super) fn flow() -> PendingEchoes {
    PendingEchoes::new(FLOW_IDENT, 0)
}

/// Returns an ICMP message of `kind` with the echo fields `ident` and `seq`,
/// `data` after them, and a valid checksum.
pub(super) fn icmp_message(kind: Icmpv4Message, ident: u16, seq: u16, data: &[u8]) -> Vec<u8> {
    let mut message = vec![0; ICMPV4_HEADER_LEN + data.len()];
    message[ICMPV4_HEADER_LEN..].copy_from_slice(data);
    let mut packet = Icmpv4Packet::new_unchecked(&mut message[..]);
    packet.set_msg_type(kind);
    packet.set_msg_code(0);
    packet.set_echo_ident(ident);
    packet.set_echo_seq_no(seq);
    packet.fill_checksum();
    message
}

/// Returns an IPv4 packet from `source` with a header of `header_len` bytes,
/// whose options are zero, that carries `payload` of `protocol`.
fn ipv4_packet(
    header_len: usize,
    protocol: IpProtocol,
    hop_limit: u8,
    source: Ipv4Addr,
    payload: &[u8],
) -> Vec<u8> {
    let mut packet = vec![0; header_len + payload.len()];
    let total_len = packet.len() as u16;
    packet[0] = 0x40 | (header_len / 4) as u8;
    packet[2..4].copy_from_slice(&total_len.to_be_bytes());
    packet[8] = hop_limit;
    packet[9] = protocol.into();
    packet[12..16].copy_from_slice(&source.octets());
    packet[header_len..].copy_from_slice(payload);
    packet
}

/// Returns what a host socket delivers for an ICMP `message` from `source`.
pub(super) fn delivered(delivery: Delivery, source: Ipv4Addr, message: &[u8]) -> Vec<u8> {
    match delivery {
        Delivery::Raw | Delivery::Packet => {
            ipv4_packet(IPV4_HEADER_LEN, IpProtocol::Icmp, 57, source, message)
        }
        Delivery::Message => message.to_vec(),
    }
}

/// Returns the identifier that the host's socket puts on a reply that answers
/// the flow's requests: the flow's own with a raw socket, and the socket's
/// binding's with a datagram socket.
pub(super) fn wire_ident(delivery: Delivery) -> u16 {
    match delivery {
        Delivery::Raw => FLOW_IDENT,
        Delivery::Packet | Delivery::Message => SOCKET_IDENT,
    }
}

/// Relays what a host socket delivers for the ICMP `message` from `source`,
/// and returns the reply and the ICMP message that the guest receives.
fn relay(
    echoes: &mut PendingEchoes,
    delivery: Delivery,
    source: Ipv4Addr,
    message: &[u8],
    now: Instant,
) -> Option<(Reply, Vec<u8>)> {
    let delivered = delivered(delivery, source, message);
    let mut buffer = vec![0; 2048];
    buffer[..delivered.len()].copy_from_slice(&delivered);
    let peer = (delivery == Delivery::Message).then_some(source);
    let reply = echoes.relay_reply(delivery, &mut buffer, delivered.len(), peer, now)?;
    buffer.truncate(reply.len);
    Some((reply, buffer))
}

/// Returns the echo reply that `host_seq` of `delivery`'s socket asks for,
/// carrying `data`.
fn echo_reply(delivery: Delivery, host_seq: u16, data: &[u8]) -> Vec<u8> {
    icmp_message(
        Icmpv4Message::EchoReply,
        wire_ident(delivery),
        host_seq,
        data,
    )
}

const DELIVERIES: [Delivery; 3] = [Delivery::Raw, Delivery::Packet, Delivery::Message];

#[test]
fn a_reply_returns_the_guests_identifier_and_sequence_number() {
    for delivery in DELIVERIES {
        let mut echoes = flow();
        let now = Instant::now();
        let host_seq = echoes.insert(GUEST_IDENT, 5, HOST, now);

        let (reply, message) = relay(
            &mut echoes,
            delivery,
            HOST,
            &echo_reply(delivery, host_seq, b"pong"),
            now,
        )
        .unwrap_or_else(|| panic!("{delivery:?}: the reply was dropped"));

        assert_eq!(reply.source, HOST, "{delivery:?}");
        let packet = Icmpv4Packet::new_checked(&message[..]).unwrap();
        assert_eq!(packet.msg_type(), Icmpv4Message::EchoReply, "{delivery:?}");
        assert_eq!(packet.echo_ident(), GUEST_IDENT, "{delivery:?}");
        assert_eq!(packet.echo_seq_no(), 5, "{delivery:?}");
        assert_eq!(packet.data(), b"pong", "{delivery:?}");
        assert!(packet.verify_checksum(), "{delivery:?}");
        assert_eq!(echoes.len(), 0, "{delivery:?}");
    }
}

#[test]
fn a_raw_socket_reply_keeps_the_hop_limit_and_a_datagram_socket_reply_gets_one() {
    let now = Instant::now();
    for (delivery, hop_limit) in [
        (Delivery::Raw, 57),
        (Delivery::Packet, 57),
        (Delivery::Message, 64),
    ] {
        let mut echoes = flow();
        let host_seq = echoes.insert(GUEST_IDENT, 1, HOST, now);
        let (reply, _) = relay(
            &mut echoes,
            delivery,
            HOST,
            &echo_reply(delivery, host_seq, b""),
            now,
        )
        .unwrap();
        assert_eq!(reply.hop_limit, hop_limit, "{delivery:?}");
    }
}

#[test]
fn requests_with_the_same_numbers_take_different_host_sequence_numbers() {
    for delivery in DELIVERIES {
        let mut echoes = flow();
        let now = Instant::now();
        let first = echoes.insert(GUEST_IDENT, 3, HOST, now);
        let second = echoes.insert(GUEST_IDENT, 3, HOST, now);
        assert_ne!(first, second, "{delivery:?}");

        // The second request's reply does not answer the first.
        let (_, message) = relay(
            &mut echoes,
            delivery,
            HOST,
            &echo_reply(delivery, second, b"second"),
            now,
        )
        .unwrap();
        assert_eq!(
            Icmpv4Packet::new_checked(&message[..]).unwrap().data(),
            b"second"
        );
        assert_eq!(echoes.len(), 1, "{delivery:?}");
        assert!(
            relay(
                &mut echoes,
                delivery,
                HOST,
                &echo_reply(delivery, first, b"first"),
                now
            )
            .is_some(),
            "{delivery:?}"
        );
    }
}

#[test]
fn a_request_has_one_reply() {
    for delivery in DELIVERIES {
        let mut echoes = flow();
        let now = Instant::now();
        let host_seq = echoes.insert(GUEST_IDENT, 1, HOST, now);
        let reply = echo_reply(delivery, host_seq, b"x");
        assert!(relay(&mut echoes, delivery, HOST, &reply, now).is_some());
        assert!(
            relay(&mut echoes, delivery, HOST, &reply, now).is_none(),
            "{delivery:?}: a duplicate reply was relayed"
        );
    }
}

#[test]
fn a_reply_must_come_from_the_host_that_was_pinged() {
    for delivery in DELIVERIES {
        let mut echoes = flow();
        let now = Instant::now();
        let host_seq = echoes.insert(GUEST_IDENT, 1, HOST, now);
        let reply = echo_reply(delivery, host_seq, b"x");
        let stranger = Ipv4Addr::new(203, 0, 113, 9);
        assert!(
            relay(&mut echoes, delivery, stranger, &reply, now).is_none(),
            "{delivery:?}"
        );
        // The stranger does not use the request up.
        assert!(
            relay(&mut echoes, delivery, HOST, &reply, now).is_some(),
            "{delivery:?}"
        );
    }
}

#[test]
fn a_raw_socket_reply_must_carry_the_identifier_that_was_sent() {
    let now = Instant::now();
    let other = |host_seq| icmp_message(Icmpv4Message::EchoReply, FLOW_IDENT ^ 1, host_seq, b"x");
    let mut echoes = flow();
    let host_seq = echoes.insert(GUEST_IDENT, 1, HOST, now);
    assert!(relay(&mut echoes, Delivery::Raw, HOST, &other(host_seq), now).is_none());
    // The guest's own identifier is no proof either: the request did not carry it.
    let guests = icmp_message(Icmpv4Message::EchoReply, GUEST_IDENT, host_seq, b"x");
    assert!(relay(&mut echoes, Delivery::Raw, HOST, &guests, now).is_none());

    // A datagram socket has an identifier of its own, which proves nothing.
    for delivery in [Delivery::Packet, Delivery::Message] {
        let mut echoes = flow();
        let host_seq = echoes.insert(GUEST_IDENT, 1, HOST, now);
        assert!(
            relay(&mut echoes, delivery, HOST, &other(host_seq), now).is_some(),
            "{delivery:?}"
        );
    }
}

#[test]
fn a_request_leaves_with_the_numbers_that_its_reply_returns() {
    for delivery in DELIVERIES {
        let mut echoes = flow();
        let now = Instant::now();
        let mut message = icmp_message(Icmpv4Message::EchoRequest, GUEST_IDENT, 5, b"ping");

        let host_seq = echoes.relay_request(delivery, &mut message, HOST, now);

        let request = Icmpv4Packet::new_checked(&message[..]).unwrap();
        assert_eq!(
            request.msg_type(),
            Icmpv4Message::EchoRequest,
            "{delivery:?}"
        );
        assert_eq!(request.echo_seq_no(), host_seq, "{delivery:?}");
        // A raw socket sends the flow's identifier. A datagram socket replaces
        // the identifier, so the request keeps the guest's.
        let ident = if delivery == Delivery::Raw {
            FLOW_IDENT
        } else {
            GUEST_IDENT
        };
        assert_eq!(request.echo_ident(), ident, "{delivery:?}");
        assert_eq!(request.data(), b"ping", "{delivery:?}");
        assert!(request.verify_checksum(), "{delivery:?}");
        assert_eq!(echoes.len(), 1, "{delivery:?}");
    }
}

#[test]
fn flows_that_see_the_same_raw_replies_do_not_take_each_others() {
    // Two guests, or two guest addresses, ping the same host with the same
    // numbers, and the host's raw sockets both see each reply.
    let now = Instant::now();
    let (mut first, mut second) = (PendingEchoes::new(1, 0), PendingEchoes::new(2, 0));
    let request = icmp_message(Icmpv4Message::EchoRequest, GUEST_IDENT, 5, b"ping");
    let (mut sent_first, mut sent_second) = (request.clone(), request);
    let first_seq = first.relay_request(Delivery::Raw, &mut sent_first, HOST, now);
    let second_seq = second.relay_request(Delivery::Raw, &mut sent_second, HOST, now);
    assert_eq!(first_seq, second_seq);

    let sent = Icmpv4Packet::new_checked(&sent_first[..]).unwrap();
    let reply = icmp_message(
        Icmpv4Message::EchoReply,
        sent.echo_ident(),
        sent.echo_seq_no(),
        b"pong",
    );
    assert!(relay(&mut second, Delivery::Raw, HOST, &reply, now).is_none());
    assert_eq!(second.len(), 1);
    let (_, message) = relay(&mut first, Delivery::Raw, HOST, &reply, now).unwrap();
    assert_eq!(
        Icmpv4Packet::new_checked(&message[..]).unwrap().data(),
        b"pong"
    );
}

#[test]
fn host_sequence_numbers_wrap_around() {
    let mut echoes = PendingEchoes::new(FLOW_IDENT, u16::MAX);
    let now = Instant::now();
    let host_seqs: Vec<u16> = (0..3)
        .map(|guest_seq| echoes.insert(GUEST_IDENT, guest_seq, HOST, now))
        .collect();
    assert_eq!(host_seqs, [u16::MAX, 0, 1]);
}

#[test]
fn only_echo_replies_are_relayed() {
    for delivery in DELIVERIES {
        let mut echoes = flow();
        let now = Instant::now();
        let host_seq = echoes.insert(GUEST_IDENT, 1, HOST, now);
        for kind in [
            Icmpv4Message::EchoRequest,
            Icmpv4Message::DstUnreachable,
            Icmpv4Message::TimeExceeded,
            Icmpv4Message::Timestamp,
        ] {
            let message = icmp_message(kind, wire_ident(delivery), host_seq, b"x");
            assert!(
                relay(&mut echoes, delivery, HOST, &message, now).is_none(),
                "{delivery:?}: {kind} was relayed"
            );
        }
        let mut with_code = echo_reply(delivery, host_seq, b"x");
        with_code[1] = 1;
        Icmpv4Packet::new_unchecked(&mut with_code[..]).fill_checksum();
        assert!(relay(&mut echoes, delivery, HOST, &with_code, now).is_none());
        assert_eq!(
            echoes.len(),
            1,
            "{delivery:?}: a stray message took the request"
        );
    }
}

#[test]
fn a_late_reply_is_dropped() {
    for delivery in DELIVERIES {
        let mut echoes = flow();
        let sent = Instant::now();
        let host_seq = echoes.insert(GUEST_IDENT, 1, HOST, sent);
        let reply = echo_reply(delivery, host_seq, b"x");
        let late = sent + ECHO_TIMEOUT + Duration::from_secs(1);
        assert!(
            relay(&mut echoes, delivery, HOST, &reply, late).is_none(),
            "{delivery:?}"
        );
    }
}

#[test]
fn the_oldest_request_is_forgotten_beyond_the_limit() {
    let mut echoes = flow();
    let start = Instant::now();
    let host_seqs: Vec<u16> = (0..=MAX_PENDING_ECHOES as u16)
        .map(|index| {
            echoes.insert(
                GUEST_IDENT,
                index,
                HOST,
                start + Duration::from_millis(index.into()),
            )
        })
        .collect();
    assert_eq!(echoes.len(), MAX_PENDING_ECHOES);

    let now = start + Duration::from_secs(1);
    let delivery = Delivery::Message;
    assert!(
        relay(
            &mut echoes,
            delivery,
            HOST,
            &echo_reply(delivery, host_seqs[0], b"x"),
            now
        )
        .is_none(),
        "the oldest request was kept"
    );
    for &host_seq in &host_seqs[1..] {
        assert!(
            relay(
                &mut echoes,
                delivery,
                HOST,
                &echo_reply(delivery, host_seq, b"x"),
                now
            )
            .is_some(),
            "a newer request was forgotten"
        );
    }
}

#[test]
fn a_request_that_did_not_leave_is_forgotten() {
    let mut echoes = flow();
    let now = Instant::now();
    let host_seq = echoes.insert(GUEST_IDENT, 1, HOST, now);
    echoes.forget(host_seq);
    assert_eq!(echoes.len(), 0);
    let delivery = Delivery::Message;
    assert!(
        relay(
            &mut echoes,
            delivery,
            HOST,
            &echo_reply(delivery, host_seq, b"x"),
            now
        )
        .is_none()
    );
}

#[test]
fn a_raw_socket_delivers_a_packet_whose_header_gives_the_sender() {
    let message = icmp_message(Icmpv4Message::EchoReply, 1, 2, b"abc");
    // A header with options, and padding after the packet.
    let mut buffer = ipv4_packet(24, IpProtocol::Icmp, 33, HOST, &message);
    let received = buffer.len() + 6;
    buffer.resize(2048, 0xee);

    let (source, hop_limit, len) =
        split_delivery(Delivery::Raw, &mut buffer, received, None).unwrap();

    assert_eq!((source, hop_limit, len), (HOST, 33, message.len()));
    assert_eq!(&buffer[..len], &message[..]);
}

#[test]
fn a_raw_socket_packet_that_is_not_ipv4_icmp_is_dropped() {
    let message = icmp_message(Icmpv4Message::EchoReply, 1, 2, b"abc");
    let wrong_protocol = ipv4_packet(IPV4_HEADER_LEN, IpProtocol::Udp, 64, HOST, &message);
    let mut wrong_version = ipv4_packet(IPV4_HEADER_LEN, IpProtocol::Icmp, 64, HOST, &message);
    wrong_version[0] = 0x65;
    let mut short_total = ipv4_packet(IPV4_HEADER_LEN, IpProtocol::Icmp, 64, HOST, &message);
    short_total[2..4].copy_from_slice(&10u16.to_be_bytes());
    let mut long_total = ipv4_packet(IPV4_HEADER_LEN, IpProtocol::Icmp, 64, HOST, &message);
    let long = long_total.len() as u16 + 1;
    long_total[2..4].copy_from_slice(&long.to_be_bytes());
    let header_only = ipv4_packet(IPV4_HEADER_LEN, IpProtocol::Icmp, 64, HOST, &[]);

    for (name, packet) in [
        ("another protocol", wrong_protocol),
        ("another version", wrong_version),
        ("a total length below the header", short_total),
        ("a total length beyond the packet", long_total),
        ("no message", header_only),
        ("a truncated header", vec![0x45; 12]),
    ] {
        let mut echoes = flow();
        let mut buffer = vec![0; 2048];
        buffer[..packet.len()].copy_from_slice(&packet);
        assert!(
            echoes
                .relay_reply(
                    Delivery::Raw,
                    &mut buffer,
                    packet.len(),
                    None,
                    Instant::now()
                )
                .is_none(),
            "{name} was relayed"
        );
    }
}

#[test]
fn a_datagram_socket_reply_needs_the_address_of_its_sender() {
    let mut echoes = flow();
    let now = Instant::now();
    let host_seq = echoes.insert(GUEST_IDENT, 1, HOST, now);
    let delivery = Delivery::Message;
    let message = echo_reply(delivery, host_seq, b"x");
    let mut buffer = vec![0; 2048];
    buffer[..message.len()].copy_from_slice(&message);
    // A message shorter than its header is not one.
    assert!(
        echoes
            .relay_reply(
                delivery,
                &mut buffer,
                ICMPV4_HEADER_LEN - 1,
                Some(HOST),
                now
            )
            .is_none()
    );
    assert!(
        echoes
            .relay_reply(delivery, &mut buffer, message.len(), None, now)
            .is_none()
    );
    assert!(
        echoes
            .relay_reply(delivery, &mut buffer, message.len(), Some(HOST), now)
            .is_some()
    );
}

/// Returns the Ethernet frame of an echo request from the guest `src` to
/// `dst`.
fn request_frame(
    src_mac: EthernetAddress,
    dst_mac: EthernetAddress,
    src: Ipv4Addr,
    dst: Ipv4Addr,
    message: &[u8],
) -> Vec<u8> {
    let mut frame = vec![0; ETHERNET_HEADER_LEN + IPV4_HEADER_LEN + message.len()];
    let mut eth = EthernetFrame::new_unchecked(&mut frame[..]);
    eth.set_src_addr(src_mac);
    eth.set_dst_addr(dst_mac);
    eth.set_ethertype(EthernetProtocol::Ipv4);
    let mut ipv4 = Ipv4Packet::new_unchecked(eth.payload_mut());
    Ipv4Repr {
        src_addr: src,
        dst_addr: dst,
        next_header: IpProtocol::Icmp,
        payload_len: message.len(),
        hop_limit: 64,
    }
    .emit(&mut ipv4, &ChecksumCapabilities::default());
    ipv4.payload_mut().copy_from_slice(message);
    frame
}

#[test]
fn the_guests_other_icmp_messages_are_dropped() {
    let (thread, driver) = DriverThread::new();
    let params = ConsommeParams::new().unwrap();
    let (guest_ip, guest_mac, gateway_mac) =
        (params.client_ip, params.client_mac, params.gateway_mac);
    let mut consomme = Consomme::new(params);
    let mut client = TestClient::new(driver);

    for kind in [
        Icmpv4Message::EchoReply,
        Icmpv4Message::Timestamp,
        Icmpv4Message::DstUnreachable,
        Icmpv4Message::Redirect,
    ] {
        let message = icmp_message(kind, GUEST_IDENT, 1, b"forged");
        let frame = request_frame(guest_mac, gateway_mac, guest_ip, HOST, &message);
        assert!(
            matches!(
                consomme.access(&mut client).send(&frame, &ChecksumState::NONE),
                Err(DropReason::UnsupportedIcmpv4(dropped)) if dropped == kind
            ),
            "{kind} was relayed"
        );
    }

    // An echo request with a code is not an echo request, nor is a message
    // without its header.
    let mut coded = icmp_message(Icmpv4Message::EchoRequest, GUEST_IDENT, 1, b"x");
    coded[1] = 1;
    let frame = request_frame(guest_mac, gateway_mac, guest_ip, HOST, &coded);
    assert!(matches!(
        consomme
            .access(&mut client)
            .send(&frame, &ChecksumState::NONE),
        Err(DropReason::UnsupportedIcmpv4(Icmpv4Message::EchoRequest))
    ));
    let frame = request_frame(guest_mac, gateway_mac, guest_ip, HOST, &coded[..4]);
    assert!(matches!(
        consomme
            .access(&mut client)
            .send(&frame, &ChecksumState::NONE),
        Err(DropReason::Packet(_))
    ));

    assert!(
        consomme.icmp.connections.is_empty(),
        "a dropped message opened a host socket"
    );
    drop((consomme, client));
    thread.join();
}

/// Returns whether the host refuses this process the ICMP socket that it
/// needs: Linux opens datagram ICMP sockets only to the groups of
/// `net.ipv4.ping_group_range`, and other hosts may keep raw sockets from
/// unprivileged processes.
fn host_refuses_icmp_sockets(error: &std::io::Error) -> bool {
    error.kind() == ErrorKind::PermissionDenied
}

#[test]
fn an_echo_request_reaches_the_host_and_its_reply_the_guest() {
    let (thread, driver) = DriverThread::new();
    let mut params = ConsommeParams::new().unwrap();
    // The host's loopback address answers, whatever the network.
    params.allow_host_local_access = true;
    let (guest_ip, guest_mac, gateway_mac) =
        (params.client_ip, params.client_mac, params.gateway_mac);
    let mut consomme = Consomme::new(params);
    let mut client = TestClient::new(driver);
    let wake = WakeChannel::new();
    wake.poll(&mut consomme, &mut client);

    let request = icmp_message(
        Icmpv4Message::EchoRequest,
        GUEST_IDENT,
        9,
        b"through the host",
    );
    let frame = request_frame(
        guest_mac,
        gateway_mac,
        guest_ip,
        Ipv4Addr::LOCALHOST,
        &request,
    );
    match consomme
        .access(&mut client)
        .send(&frame, &ChecksumState::NONE)
    {
        Ok(()) => {}
        Err(DropReason::Io(error)) if host_refuses_icmp_sockets(&error) => {
            tracing::warn!(%error, "the host refuses this process an ICMP socket; skipping");
            drop((consomme, client));
            thread.join();
            return;
        }
        Err(error) => panic!("the echo request was dropped: {error}"),
    }

    // The poll registers the new host socket with the waker, and reads a reply
    // that is already there, which a loopback echo may be.
    wake.poll(&mut consomme, &mut client);
    if client.received_packets.is_empty() {
        wake.wait_for_packet(&mut consomme, &mut client, |_| true);
    }
    let frame = EthernetFrame::new_checked(&client.received_packets[0]).unwrap();
    assert_eq!(frame.dst_addr(), guest_mac);
    let ipv4 = Ipv4Packet::new_checked(frame.payload()).unwrap();
    assert_eq!(ipv4.next_header(), IpProtocol::Icmp);
    assert_eq!(ipv4.src_addr(), Ipv4Addr::LOCALHOST);
    assert_eq!(ipv4.dst_addr(), guest_ip);
    assert!(ipv4.verify_checksum());
    let reply = Icmpv4Packet::new_checked(ipv4.payload()).unwrap();
    assert_eq!(reply.msg_type(), Icmpv4Message::EchoReply);
    assert_eq!(reply.echo_ident(), GUEST_IDENT);
    assert_eq!(reply.echo_seq_no(), 9);
    assert_eq!(reply.data(), b"through the host");
    assert!(reply.verify_checksum());
    // The request is answered, so nothing awaits a reply.
    assert_eq!(
        consomme
            .icmp
            .connections
            .values()
            .map(|connection| connection.echoes.len())
            .sum::<usize>(),
        0
    );
    drop((consomme, client));
    thread.join();
}
