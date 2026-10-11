// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of the translated peers that a guest UDP flow remembers.

use super::*;

fn connection() -> UdpConnection {
    UdpConnection {
        socket: None,
        host_port: 0,
        guest_mac: EthernetAddress([0; 6]),
        stats: Default::default(),
        recycle: false,
        last_activity: Instant::now(),
        gso_size: None,
        translated_peers: HashMap::new(),
    }
}

fn gateway(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::new(10, 0, 0, 1), port))
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

#[test]
fn translated_peers_follow_the_latest_address() {
    let timeout = Duration::from_secs(30);
    let now = Instant::now();
    let mut connection = connection();

    connection.remember_translated_peer(loopback(1), loopback(1), now, timeout);
    assert!(connection.translated_peers.is_empty());

    connection.remember_translated_peer(loopback(1), gateway(1), now, timeout);
    assert_eq!(
        connection.translated_peer(&loopback(1), now, timeout),
        Some(gateway(1))
    );
    let virtual_peer = SocketAddr::from((Ipv4Addr::new(10, 0, 0, 254), 1));
    connection.remember_translated_peer(loopback(1), virtual_peer, now, timeout);
    assert_eq!(
        connection.translated_peer(&loopback(1), now, timeout),
        Some(virtual_peer)
    );

    // A guest that addresses the host address itself is answered from it.
    connection.remember_translated_peer(loopback(1), loopback(1), now, timeout);
    assert_eq!(connection.translated_peer(&loopback(1), now, timeout), None);
    assert!(connection.translated_peers.is_empty());
}

#[test]
fn translated_peers_expire_unless_refreshed() {
    let timeout = Duration::from_secs(30);
    let now = Instant::now();
    let mut connection = connection();
    connection.remember_translated_peer(loopback(1), gateway(1), now, timeout);
    connection.remember_translated_peer(loopback(2), gateway(2), now, timeout);

    // A reply refreshes its peer.
    let refreshed = now + timeout;
    assert_eq!(
        connection.translated_peer(&loopback(1), refreshed, timeout),
        Some(gateway(1))
    );
    let later = refreshed + Duration::from_secs(1);
    assert_eq!(
        connection.translated_peer(&loopback(1), later, timeout),
        Some(gateway(1))
    );
    assert_eq!(
        connection.translated_peer(&loopback(2), later, timeout),
        None
    );
}

#[test]
fn translated_peers_stay_bounded() {
    let timeout = Duration::from_secs(30);
    let now = Instant::now();
    let mut connection = connection();
    for port in 1..=MAX_TRANSLATED_PEERS as u16 {
        connection.remember_translated_peer(
            loopback(port),
            gateway(port),
            now + Duration::from_millis(port.into()),
            timeout,
        );
    }
    assert_eq!(connection.translated_peers.len(), MAX_TRANSLATED_PEERS);

    // A new peer displaces the one seen longest ago.
    let later = now + Duration::from_secs(1);
    connection.remember_translated_peer(loopback(0), gateway(0), later, timeout);
    assert_eq!(connection.translated_peers.len(), MAX_TRANSLATED_PEERS);
    assert_eq!(
        connection.translated_peer(&loopback(1), later, timeout),
        None
    );
    for port in 0..=MAX_TRANSLATED_PEERS as u16 {
        if port != 1 {
            assert_eq!(
                connection.translated_peer(&loopback(port), later, timeout),
                Some(gateway(port))
            );
        }
    }

    // Expired peers make room before any live one is displaced.
    let expired = later + timeout + Duration::from_secs(1);
    connection.remember_translated_peer(loopback(1), gateway(1), expired, timeout);
    assert_eq!(connection.translated_peers.len(), 1);
}
