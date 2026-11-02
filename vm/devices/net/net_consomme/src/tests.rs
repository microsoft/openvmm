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
        let start = if ipv6 { 54 } else { 34 };
        let header_len = start + 24;
        let mut header = vec![0; header_len];
        header[start + 12] = 6 << 4;
        let payload = vec![0xa5; 3001];
        let mut pool = Bufs::new(GuestMemory::allocate(65536));
        pool.set_capacity(RxId(0), 65536);
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
        assert_eq!(state.rx_ready.pop_front().unwrap().0, 0);
        let mut packet = vec![0; metadata.len];
        pool.guest_memory().read_at(0, &mut packet).unwrap();
        assert_eq!(&packet[..header_len], header);
        assert_eq!(&packet[header_len..], payload);
    }
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
                assert_eq!(client.rx_gso_supported(false), checksum && tcpv4_gso);
                assert_eq!(client.rx_gso_supported(true), checksum && tcpv6_gso);
            }
        }
    }
}
