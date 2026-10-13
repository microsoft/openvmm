// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use crate::Consomme;
use crate::ConsommeParams;
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
    let mut consomme = Consomme::new(params);
    consomme.icmp.max_connections = 1;

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
