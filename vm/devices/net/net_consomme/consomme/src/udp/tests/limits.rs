// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of the UDP active-flow limit.

use super::*;
use crate::limits::FlowLimits;

/// Creates a Consomme instance with the given UDP idle timeout and active-flow
/// limits.
fn create_consomme_with_flow_limits(timeout: Duration, flow_limits: FlowLimits) -> Consomme {
    let mut params = ConsommeParams::new().expect("Failed to create params");
    params.udp_timeout = timeout;
    params.flow_limits = flow_limits;
    Consomme::new(params)
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
