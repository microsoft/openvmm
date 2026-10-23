// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use crate::Consomme;
use crate::ConsommeParams;
use crate::limits::DEFAULT_MAX_ACTIVE_ICMP_FLOWS;
use crate::limits::FlowLimits;
use pal_async::DefaultDriver;
use pal_async::driver::Driver;

struct TestClient {
    driver: DefaultDriver,
}

impl Client for TestClient {
    fn driver(&self) -> &dyn Driver {
        &self.driver
    }

    fn recv(&mut self, _data: &[u8], _checksum: &ChecksumState) {}

    fn rx_mtu(&mut self) -> usize {
        MIN_MTU
    }
}

#[pal_async::async_test]
async fn test_icmp_connection_limit_rejects_and_recovers(driver: DefaultDriver) {
    let timeout = Duration::from_millis(1);
    let mut params = ConsommeParams::new().unwrap();
    params.udp_timeout = timeout;
    params.flow_limits.icmp = Some(1);
    let mut consomme = Consomme::new(params);

    let existing_addr = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 0);
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
    let socket = PolledSocket::new(&driver, socket).unwrap();
    consomme.icmp.connections.insert(
        existing_addr,
        IcmpConnection {
            socket,
            guest_mac: EthernetAddress([0, 0, 0, 0, 0, 1]),
            stats: Default::default(),
            last_activity: Instant::now() - 2 * timeout,
        },
    );

    let new_addr = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 3), 0);
    assert!(matches!(
        consomme.icmp.check_flow_limit(&new_addr),
        Err(DropReason::IcmpConnectionLimit)
    ));

    let mut client = TestClient { driver };
    let mut cx = Context::from_waker(std::task::Waker::noop());
    consomme.access(&mut client).poll_icmp(&mut cx);

    assert!(consomme.icmp.connections.is_empty());
    assert!(consomme.icmp.check_flow_limit(&new_addr).is_ok());
}

/// Fills `consomme` with `count` active ICMP source flows, each holding a UDP
/// socket in place of a host ICMP socket.
fn insert_flows(consomme: &mut Consomme, driver: &DefaultDriver, count: usize) {
    for host in (1..).take(count) {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        consomme.icmp.connections.insert(
            SocketAddrV4::new(Ipv4Addr::new(10, 0, 1, host), 0),
            IcmpConnection {
                socket: PolledSocket::new(driver, socket).unwrap(),
                guest_mac: EthernetAddress([0, 0, 0, 0, 0, 1]),
                stats: Default::default(),
                last_activity: Instant::now(),
            },
        );
    }
}

#[pal_async::async_test]
async fn test_icmp_unbounded_flow_limit_admits_beyond_default(driver: DefaultDriver) {
    let new_addr = SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 1), 0);

    let mut params = ConsommeParams::new().unwrap();
    params.flow_limits = FlowLimits::UNBOUNDED;
    let mut unbounded = Consomme::new(params);
    insert_flows(&mut unbounded, &driver, DEFAULT_MAX_ACTIVE_ICMP_FLOWS);
    assert!(
        unbounded.icmp.check_flow_limit(&new_addr).is_ok(),
        "an unbounded ICMP flow map must admit a flow beyond the default limit"
    );

    let mut bounded = Consomme::new(ConsommeParams::new().unwrap());
    insert_flows(&mut bounded, &driver, DEFAULT_MAX_ACTIVE_ICMP_FLOWS);
    assert!(matches!(
        bounded.icmp.check_flow_limit(&new_addr),
        Err(DropReason::IcmpConnectionLimit)
    ));
}
