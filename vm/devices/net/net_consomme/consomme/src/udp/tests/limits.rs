// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of the UDP active-flow limit.

use super::*;
use crate::limits::DEFAULT_MAX_ACTIVE_UDP_FLOWS;
use crate::limits::FlowLimits;

/// Creates a Consomme instance with the given UDP idle timeout and active-flow
/// limits.
fn create_consomme_with_flow_limits(timeout: Duration, flow_limits: FlowLimits) -> Consomme {
    let mut params = ConsommeParams::new().expect("Failed to create params");
    params.udp_timeout = timeout;
    params.flow_limits = flow_limits;
    Consomme::new(params)
}

/// Fills `consomme` with `count` active UDP flows that hold no host socket.
fn insert_socketless_flows(consomme: &mut Consomme, count: usize) {
    for port in (10000..).take(count) {
        consomme.udp.connections.insert(
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), port)),
            UdpConnection {
                socket: None,
                host_port: 0,
                guest_mac: EthernetAddress([0, 0, 0, 0, 0, 1]),
                stats: Default::default(),
                recycle: false,
                last_activity: Instant::now(),
                gso_size: None,
            },
        );
    }
}

#[pal_async::async_test]
async fn test_udp_connection_limit_rejects_and_recovers(driver: DefaultDriver) {
    let driver = Arc::new(driver);
    let mut consomme = create_consomme_with_flow_limits(
        Duration::from_millis(1),
        FlowLimits {
            udp: Some(2),
            ..FlowLimits::default()
        },
    );
    let mut client = TestClient::new(driver);
    let mut access = consomme.access(&mut client);

    for port in [10001, 10002] {
        access
            .get_or_insert(
                SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), port)),
                None,
            )
            .unwrap();
    }
    assert_eq!(access.udp_connection_count(), 2);

    let rejected = access.get_or_insert(
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 10003)),
        None,
    );
    assert!(matches!(rejected, Err(DropReason::UdpConnectionLimit)));
    assert_eq!(
        access.udp_connection_count(),
        2,
        "a rejected flow must not create a host UDP socket"
    );

    for connection in access.inner.udp.connections.values_mut() {
        connection.last_activity = Instant::now() - Duration::from_millis(2);
    }
    let mut cx = Context::from_waker(std::task::Waker::noop());
    access.poll_udp(&mut cx);
    assert_eq!(access.udp_connection_count(), 0);

    access
        .get_or_insert(
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 10003)),
            None,
        )
        .unwrap();
    assert_eq!(access.udp_connection_count(), 1);
}

#[pal_async::async_test]
async fn test_udp_unbounded_flow_limit_admits_beyond_default(driver: DefaultDriver) {
    let driver = Arc::new(driver);
    let new_flow = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 20000));

    let mut unbounded =
        create_consomme_with_flow_limits(Duration::from_secs(30), FlowLimits::UNBOUNDED);
    insert_socketless_flows(&mut unbounded, DEFAULT_MAX_ACTIVE_UDP_FLOWS);
    let mut client = TestClient::new(driver.clone());
    let mut access = unbounded.access(&mut client);
    access.get_or_insert(new_flow, None).unwrap();
    assert_eq!(
        access.udp_connection_count(),
        DEFAULT_MAX_ACTIVE_UDP_FLOWS + 1,
        "an unbounded UDP flow map must admit a flow beyond the default limit"
    );

    let mut bounded =
        create_consomme_with_flow_limits(Duration::from_secs(30), FlowLimits::default());
    insert_socketless_flows(&mut bounded, DEFAULT_MAX_ACTIVE_UDP_FLOWS);
    let mut client = TestClient::new(driver);
    let mut access = bounded.access(&mut client);
    assert!(matches!(
        access.get_or_insert(new_flow, None),
        Err(DropReason::UdpConnectionLimit)
    ));
    assert_eq!(access.udp_connection_count(), DEFAULT_MAX_ACTIVE_UDP_FLOWS);
}
