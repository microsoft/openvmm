// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Egress policy tests of the TAP endpoint.

use super::configure_tap;
use super::make_pool;
use super::new_endpoint;
use net_backend::Endpoint;
use net_backend::QueueConfig;
use net_backend::RxId;
use net_backend::TxId;
use net_backend::TxMetadata;
use net_backend::TxSegment;
use net_backend::TxSegmentType;
use net_backend_resources::egress::EgressPolicy;
use net_backend_resources::egress::EgressPolicyMode;
use net_backend_resources::mac_address::MacAddress;
use pal_async::DefaultDriver;
use std::future::poll_fn;

fn endpoint_arp_request(target: std::net::Ipv4Addr) -> Vec<u8> {
    let guest_mac = [0x52, 0x54, 0, 0, 0, 2];
    let mut frame = vec![0u8; 42];
    frame[..6].fill(0xff);
    frame[6..12].copy_from_slice(&guest_mac);
    frame[12..14].copy_from_slice(&0x0806u16.to_be_bytes());
    frame[14..16].copy_from_slice(&1u16.to_be_bytes());
    frame[16..18].copy_from_slice(&0x0800u16.to_be_bytes());
    frame[18] = 6;
    frame[19] = 4;
    frame[20..22].copy_from_slice(&1u16.to_be_bytes());
    frame[22..28].copy_from_slice(&guest_mac);
    frame[28..32].copy_from_slice(&[10, 0, 0, 2]);
    frame[38..42].copy_from_slice(&target.octets());
    frame
}

/// Validates that exact endpoint policy forwards an on-link ARP request to TAP.
pub(super) async fn test_tap_endpoint_policy_forwards_on_link_arp(driver: DefaultDriver) {
    let target = std::net::Ipv4Addr::new(10, 0, 0, 9);
    let mut endpoint = new_endpoint("tap_policy").unwrap();
    configure_tap("tap_policy", "10.0.0.9/24");
    endpoint
        .set_egress_policy(
            EgressPolicy::bind(
                std::net::Ipv4Addr::new(10, 0, 0, 2),
                24,
                MacAddress::new([0x52, 0x54, 0, 0, 0, 2]),
                std::net::Ipv4Addr::new(10, 0, 0, 1),
                EgressPolicyMode::TcpEndpoints(vec!["10.0.0.9:443".parse().unwrap()]),
            )
            .unwrap(),
        )
        .unwrap();

    let (mut pool, mem) = make_pool();
    let initial_rx: Vec<_> = (1..128).map(RxId).collect();
    let mut queues = Vec::new();
    endpoint
        .get_queues(
            vec![QueueConfig {
                driver: Box::new(driver),
            }],
            None,
            &mut queues,
        )
        .await
        .unwrap();
    let queue = &mut queues[0];
    queue.rx_avail(&mut pool, &initial_rx);

    let frame = endpoint_arp_request(target);
    mem.write_at(0, &frame).unwrap();
    let segments = [TxSegment {
        ty: TxSegmentType::Head(TxMetadata {
            id: TxId(0),
            segment_count: 1,
            len: frame.len() as u32,
            ..Default::default()
        }),
        gpa: 0,
        len: frame.len() as u32,
    }];
    let (completed, count) = queue.tx_avail(&mut pool, &segments).unwrap();
    assert_eq!(count, 1);
    if !completed {
        poll_fn(|cx| queue.poll_ready(cx, &mut pool)).await;
        let mut done = [TxId(0)];
        assert_eq!(queue.tx_poll(&mut pool, &mut done).unwrap(), 1);
    }

    poll_fn(|cx| queue.poll_ready(cx, &mut pool)).await;
    let mut packets = [RxId(0); 128];
    let n = queue.rx_poll(&mut pool, &mut packets).unwrap();
    let found_reply = packets[..n].iter().any(|rx_id| {
        let mut reply = [0u8; 42];
        mem.read_at(rx_id.0 as u64 * 2048, &mut reply).unwrap();
        reply[12..14] == 0x0806u16.to_be_bytes()
            && reply[20..22] == 2u16.to_be_bytes()
            && reply[28..32] == target.octets()
            && reply[32..38] == [0x52, 0x54, 0, 0, 0, 2]
            && reply[38..42] == [10, 0, 0, 2]
    });
    assert!(
        found_reply,
        "expected TAP to answer the allowed endpoint ARP"
    );
}
