// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of IPv4 fragment rejection.

use super::*;

/// IPv4 fragments are dropped before checksum validation or host socket work.
#[pal_async::async_test]
async fn ipv4_fragments_are_rejected(driver: DefaultDriver) {
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
        Ipv4Address::new(192, 0, 2, 1),
    );
    Ipv4Packet::new_unchecked(&mut buf[ETHERNET_HEADER_LEN..len]).set_more_frags(true);

    let result = consomme
        .access(&mut client)
        .send(&buf[..len], &ChecksumState::NONE);
    assert!(
        matches!(result, Err(DropReason::FragmentedPacket)),
        "fragmented IPv4 traffic should be rejected, got {result:?}"
    );
}
