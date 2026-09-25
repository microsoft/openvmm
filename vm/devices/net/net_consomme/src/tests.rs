// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of the egress policy of the Consomme endpoint.

use super::*;
use guestmem::GuestMemory;
use net_backend::Endpoint as _;
use net_backend::QueueConfig;
use net_backend::TxMetadata;
use net_backend::TxSegmentType;
use net_backend_resources::egress::EgressPolicyMode;
use net_backend_resources::mac_address::MacAddress;
use pal_async::DefaultDriver;
use std::future::poll_fn;

const GUEST_IPV4: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 2);
const GATEWAY_IPV4: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);
const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0, 0, 0, 2];
const GATEWAY_MAC: [u8; 6] = [0x52, 0x54, 0, 0, 0, 1];

fn arp_request(target: Ipv4Addr) -> Vec<u8> {
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
    frame[38..42].copy_from_slice(&target.octets());
    frame
}

fn tx_segment(id: TxId, frame_length: usize) -> TxSegment {
    TxSegment {
        ty: TxSegmentType::Head(TxMetadata {
            id,
            segment_count: 1,
            len: frame_length as u32,
            ..Default::default()
        }),
        gpa: 0,
        len: frame_length as u32,
    }
}

#[pal_async::async_test]
async fn endpoint_policy_proxy_arps_only_bound_on_link_targets(driver: DefaultDriver) {
    let mut params = ConsommeParams::new().unwrap();
    params
        .set_static_ipv4(GUEST_IPV4, 24, GATEWAY_IPV4, GATEWAY_MAC)
        .unwrap();
    params.client_mac.0 = GUEST_MAC;
    let mut endpoint = ConsommeEndpoint::new(params);
    endpoint
        .set_egress_policy(
            EgressPolicy::bind(
                GUEST_IPV4,
                24,
                MacAddress::new(GUEST_MAC),
                GATEWAY_IPV4,
                EgressPolicyMode::TcpEndpoints(vec![
                    "10.0.0.9:443".parse().unwrap(),
                    "192.0.2.7:443".parse().unwrap(),
                ]),
            )
            .unwrap(),
        )
        .unwrap();

    let layout = net_backend::tests::test_layout();
    let memory = GuestMemory::allocate(layout.end_of_ram() as usize);
    let mut pool = net_backend::tests::Bufs::new(memory.clone());
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

    let allowed = arp_request(Ipv4Addr::new(10, 0, 0, 9));
    memory.write_at(0, &allowed).unwrap();
    queue.rx_avail(&mut pool, &[RxId(1)]);
    assert_eq!(
        queue
            .tx_avail(&mut pool, &[tx_segment(TxId(7), allowed.len())])
            .unwrap(),
        (false, 1)
    );
    poll_fn(|cx| queue.poll_ready(cx, &mut pool)).await;

    let mut completed = [TxId(0)];
    assert_eq!(queue.tx_poll(&mut pool, &mut completed).unwrap(), 1);
    assert_eq!(completed[0].0, 7);
    let mut received = [RxId(0)];
    assert_eq!(queue.rx_poll(&mut pool, &mut received).unwrap(), 1);
    assert_eq!(received[0].0, 1);
    let metadata = pool.rx_metadata(RxId(1)).unwrap();
    assert_eq!(metadata.len, 42);
    let mut reply = vec![0u8; metadata.len];
    memory.read_at(2048, &mut reply).unwrap();
    assert_eq!(&reply[..6], &GUEST_MAC);
    assert_eq!(&reply[6..12], &GATEWAY_MAC);
    assert_eq!(u16::from_be_bytes([reply[20], reply[21]]), 2);
    assert_eq!(&reply[22..28], &GATEWAY_MAC);
    assert_eq!(&reply[28..32], &Ipv4Addr::new(10, 0, 0, 9).octets());
    assert_eq!(&reply[32..38], &GUEST_MAC);
    assert_eq!(&reply[38..42], &GUEST_IPV4.octets());

    let denied = arp_request(Ipv4Addr::new(10, 0, 0, 10));
    memory.write_at(0, &denied).unwrap();
    queue.rx_avail(&mut pool, &[RxId(2)]);
    queue
        .tx_avail(&mut pool, &[tx_segment(TxId(8), denied.len())])
        .unwrap();
    poll_fn(|cx| queue.poll_ready(cx, &mut pool)).await;
    assert_eq!(queue.tx_poll(&mut pool, &mut completed).unwrap(), 1);
    assert_eq!(completed[0].0, 8);
    assert_eq!(queue.rx_poll(&mut pool, &mut received).unwrap(), 0);

    let mut spoofed = allowed;
    spoofed[11] ^= 1;
    memory.write_at(0, &spoofed).unwrap();
    queue
        .tx_avail(&mut pool, &[tx_segment(TxId(9), spoofed.len())])
        .unwrap();
    poll_fn(|cx| queue.poll_ready(cx, &mut pool)).await;
    assert_eq!(queue.tx_poll(&mut pool, &mut completed).unwrap(), 1);
    assert_eq!(completed[0].0, 9);
    assert_eq!(queue.rx_poll(&mut pool, &mut received).unwrap(), 0);

    drop(queues);
    endpoint.stop().await;
}
