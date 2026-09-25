// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of the TCP active-flow limit.

use super::*;

#[cfg(unix)]
#[pal_async::async_test]
async fn test_tcp_connection_limit_rejects_and_recovers(driver: DefaultDriver) {
    let mut harness = TcpTestHarness::connect(driver).await;
    harness.consomme.tcp.max_connections = 1;
    assert_eq!(harness.consomme.tcp.connections.len(), 1);

    let syn = TcpRepr {
        src_port: harness.guest_port + 1,
        dst_port: harness.dst_port,
        control: TcpControl::Syn,
        seq_number: TcpSeqNumber(2000),
        ack_number: None,
        window_len: 64240,
        window_scale: Some(7),
        max_seg_size: Some(1460),
        sack_permitted: false,
        sack_ranges: [None, None, None],
        timestamp: None,
        payload: &[],
    };
    let len = build_tcp_packet(
        &mut harness.buf,
        harness.guest_mac,
        harness.gateway_mac,
        harness.guest_ip,
        harness.dst_ip,
        &syn,
    );
    let rejected = harness
        .consomme
        .access(&mut harness.client)
        .send(&harness.buf[..len], &ChecksumState::NONE);
    assert!(matches!(rejected, Err(DropReason::TcpConnectionLimit)));
    assert_eq!(
        harness.consomme.tcp.connections.len(),
        1,
        "a rejected flow must not create a host TCP socket or buffer"
    );

    // Simulate the normal poll-time removal after a connection closes.
    harness.consomme.tcp.connections.clear();
    harness
        .consomme
        .access(&mut harness.client)
        .send(&harness.buf[..len], &ChecksumState::NONE)
        .unwrap();
    assert_eq!(harness.consomme.tcp.connections.len(), 1);
}
