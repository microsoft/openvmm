// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use consomme::Client as _;
use guestmem::GuestMemory;
use net_backend::tests::Bufs;
use pal_async::DefaultDriver;
use test_with_tracing::test;

#[pal_async::async_test]
async fn receive_gso_metadata(driver: DefaultDriver) {
    for ipv6 in [false, true] {
        for mergeable in [false, true] {
            let start = if ipv6 { 54 } else { 34 };
            let header_len = start + 24;
            let mut header = vec![0; header_len];
            header[start + 12] = 6 << 4;
            let payload = vec![0xa5; 3001];
            let mut pool = Bufs::new(GuestMemory::allocate(65536));
            if mergeable {
                pool = pool.with_mergeable_buffers();
                pool.set_capacity(RxId(0), header_len as u32);
                pool.set_capacity(RxId(1), 1001);
                pool.set_capacity(RxId(2), 2000);
            } else {
                pool.set_capacity(RxId(0), 65536);
            }
            let mut state = QueueState {
                rx_avail: if mergeable {
                    [RxId(0), RxId(1), RxId(2)].into()
                } else {
                    [RxId(0)].into()
                },
                rx_ready: VecDeque::new(),
                tx_avail: VecDeque::new(),
                tx_ready: VecDeque::new(),
                tx_scratch: Vec::new(),
            };
            let mut stats = Stats::default();
            let mut client = Client {
                state: &mut state,
                stats: &mut stats,
                driver: &driver,
                pool: &mut pool,
                enabled_rx_offloads: RxOffloadSupport {
                    checksum: true,
                    tcpv4_gso: !ipv6,
                    tcpv6_gso: ipv6,
                },
            };
            client.recv_segments(
                &[&header, &payload[..1], &payload[1..]],
                &ChecksumState {
                    ipv4: !ipv6,
                    tcp: true,
                    udp: false,
                    partial: true,
                    tso: Some(1460),
                    gso: None,
                },
            );
            let metadata = pool.rx_metadata(RxId(0)).unwrap();
            assert_eq!(metadata.len, header_len + payload.len());
            assert_eq!(metadata.l4_protocol, L4Protocol::Tcp);
            assert_eq!(metadata.l4_checksum, RxChecksumState::Unknown);
            assert_eq!(
                metadata.ip_checksum,
                if ipv6 {
                    RxChecksumState::Unknown
                } else {
                    RxChecksumState::Good
                }
            );
            let gso = metadata.offload.gso().unwrap();
            assert_eq!(
                gso.protocol,
                if ipv6 {
                    RxGsoProtocol::TcpV6
                } else {
                    RxGsoProtocol::TcpV4
                }
            );
            assert_eq!(gso.header_len as usize, header_len);
            assert_eq!(gso.max_segment_size, 1460);
            let checksum = metadata.offload.checksum_metadata().unwrap();
            assert_eq!(checksum.start as usize, start);
            assert_eq!(checksum.offset, 16);
            let ids = state.rx_ready.pop_front().unwrap();
            assert_eq!(
                ids.iter().map(|id| id.0).collect::<Vec<_>>(),
                if mergeable { vec![0, 1, 2] } else { vec![0] }
            );
            let mut packet = vec![0; metadata.len];
            if mergeable {
                pool.guest_memory()
                    .read_at(0, &mut packet[..header_len])
                    .unwrap();
                pool.guest_memory()
                    .read_at(2048, &mut packet[header_len..header_len + 1001])
                    .unwrap();
                pool.guest_memory()
                    .read_at(4096, &mut packet[header_len + 1001..])
                    .unwrap();
            } else {
                pool.guest_memory().read_at(0, &mut packet).unwrap();
            }
            assert_eq!(&packet[..header_len], header);
            assert_eq!(&packet[header_len..], payload);
        }
    }
}

#[pal_async::async_test]
async fn receive_partial_checksum_metadata_without_gso(driver: DefaultDriver) {
    for ipv6 in [false, true] {
        for tcp in [false, true] {
            for partial in [false, true] {
                let start = if ipv6 { 54 } else { 34 };
                let header_len = start + if tcp { 20 } else { 8 };
                let packet = vec![0xa5; header_len + 101];
                let mut pool = Bufs::new(GuestMemory::allocate(4096));
                pool.set_capacity(RxId(0), 1514);
                let mut state = QueueState {
                    rx_avail: [RxId(0)].into(),
                    rx_ready: VecDeque::new(),
                    tx_avail: VecDeque::new(),
                    tx_ready: VecDeque::new(),
                    tx_scratch: Vec::new(),
                };
                let mut stats = Stats::default();
                let mut client = Client {
                    state: &mut state,
                    stats: &mut stats,
                    driver: &driver,
                    pool: &mut pool,
                    enabled_rx_offloads: RxOffloadSupport {
                        checksum: partial,
                        ..Default::default()
                    },
                };
                client.recv_segments(
                    &[&packet[..header_len], &packet[header_len..]],
                    &ChecksumState {
                        ipv4: !ipv6,
                        tcp,
                        udp: !tcp,
                        partial,
                        tso: None,
                        gso: None,
                    },
                );
                let metadata = pool.rx_metadata(RxId(0)).unwrap();
                assert_eq!(metadata.len, packet.len());
                assert_eq!(
                    metadata.l4_protocol,
                    if tcp {
                        L4Protocol::Tcp
                    } else {
                        L4Protocol::Udp
                    }
                );
                assert_eq!(
                    metadata.l4_checksum,
                    if partial {
                        RxChecksumState::Unknown
                    } else {
                        RxChecksumState::Good
                    }
                );
                assert!(metadata.offload.gso().is_none());
                if partial {
                    let checksum = metadata.offload.checksum_metadata().unwrap();
                    assert_eq!(checksum.start as usize, start);
                    assert_eq!(checksum.offset, if tcp { 16 } else { 6 });
                } else {
                    assert!(metadata.offload.checksum_metadata().is_none());
                }
                assert_eq!(state.rx_ready.pop_front().unwrap()[0].0, 0);
                let mut received = vec![0; packet.len()];
                pool.guest_memory().read_at(0, &mut received).unwrap();
                assert_eq!(received, packet);
            }
        }
    }
}

#[pal_async::async_test]
async fn mergeable_receive_uses_aggregate_capacity(driver: DefaultDriver) {
    let mut pool = Bufs::new(GuestMemory::allocate(0x10000)).with_mergeable_buffers();
    pool.set_capacity(RxId(7), 0);
    pool.set_capacity(RxId(3), 1000);
    pool.set_capacity(RxId(1), 1000);
    let mut state = QueueState {
        rx_avail: VecDeque::from([RxId(7)]),
        rx_ready: VecDeque::new(),
        tx_avail: VecDeque::new(),
        tx_ready: VecDeque::new(),
        tx_scratch: Vec::new(),
    };
    let mut stats = Stats::default();
    let mut client = Client {
        state: &mut state,
        stats: &mut stats,
        driver: &driver,
        pool: &mut pool,
        enabled_rx_offloads: RxOffloadSupport::default(),
    };
    assert_eq!(client.rx_mtu(), 0);
    client.state.rx_avail.push_back(RxId(3));
    assert_eq!(client.rx_mtu(), 0);
    client.state.rx_avail.push_back(RxId(1));
    assert_eq!(client.rx_mtu(), consomme::MIN_MTU);
    client.enabled_rx_offloads = RxOffloadSupport {
        checksum: true,
        tcpv4_gso: true,
        tcpv6_gso: true,
    };
    assert_eq!(client.rx_mtu(), 2000);
    client.recv_segments(
        &[&[0x5a; 1000], &[0x42; 500]],
        &ChecksumState {
            ipv4: true,
            tcp: true,
            udp: false,
            partial: false,
            tso: None,
            gso: None,
        },
    );
    assert!(client.state.rx_avail.is_empty());
    let ids = client.state.rx_ready.pop_front().unwrap();
    assert_eq!(ids.iter().map(|id| id.0).collect::<Vec<_>>(), [7, 3, 1]);
    assert_eq!(client.rx_mtu(), 0);
    assert_eq!(pool.rx_metadata(RxId(7)).unwrap().len, 1500);
}

#[pal_async::async_test]
async fn receive_gso_requires_negotiated_checksum_and_ip_version(driver: DefaultDriver) {
    let mut pool = Bufs::new(GuestMemory::empty());
    let mut state = QueueState {
        rx_avail: VecDeque::new(),
        rx_ready: VecDeque::new(),
        tx_avail: VecDeque::new(),
        tx_ready: VecDeque::new(),
        tx_scratch: Vec::new(),
    };
    let mut stats = Stats::default();
    for checksum in [false, true] {
        for tcpv4_gso in [false, true] {
            for tcpv6_gso in [false, true] {
                let client = Client {
                    state: &mut state,
                    stats: &mut stats,
                    driver: &driver,
                    pool: &mut pool,
                    enabled_rx_offloads: RxOffloadSupport {
                        checksum,
                        tcpv4_gso,
                        tcpv6_gso,
                    },
                };
                assert_eq!(client.rx_checksum_supported(), checksum);
                assert_eq!(client.rx_gso_supported(false), checksum && tcpv4_gso);
                assert_eq!(client.rx_gso_supported(true), checksum && tcpv6_gso);
            }
        }
    }
}
