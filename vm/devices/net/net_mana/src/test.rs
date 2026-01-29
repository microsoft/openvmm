// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(test)]

use crate::GuestDmaMode;
use crate::ManaEndpoint;
use crate::ManaTestConfiguration;
use crate::QueueStats;
use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use gdma::VportConfig;
use gdma_defs::bnic::ManaQueryDeviceCfgResp;
use inspect_counters::Counter;
use mana_driver::mana::ManaDevice;
use mesh::CancelContext;
use mesh::CancelReason;
use net_backend::BufferAccess;
use net_backend::Endpoint;
use net_backend::QueueConfig;
use net_backend::RxId;
use net_backend::TxId;
use net_backend::TxSegment;
use net_backend::loopback::LoopbackEndpoint;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pci_core::msi::MsiInterruptSet;
use std::future::poll_fn;
use std::time::Duration;
use test_with_tracing::test;
use user_driver_emulated_mock::DeviceTestMemory;
use user_driver_emulated_mock::EmulatedDevice;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;

const IPV4_HEADER_LENGTH: usize = 54;
const MAX_GDMA_SGE_PER_TX_PACKET: usize = 31;

struct TxPacketBuilder {
    /// Tracks segments for all the packets
    segments: Vec<TxSegment>,
    /// Total length of all the segments
    total_len: u64,
    /// Tracks the length of each packet. The length of this vector is the number of packets.
    pkt_len: Vec<u64>,
}

impl TxPacketBuilder {
    fn new() -> Self {
        Self {
            segments: Vec::new(),
            total_len: 0,
            pkt_len: Vec::new(),
        }
    }

    fn push(&mut self, segment: TxSegment) {
        self.total_len += segment.len as u64;
        if let net_backend::TxSegmentType::Head(metadata) = &segment.ty {
            self.pkt_len.push(metadata.len as u64);
        }
        self.segments.push(segment);
    }

    fn packet_data(&self) -> Vec<u8> {
        (0..self.total_len).map(|v| v as u8).collect::<Vec<u8>>()
    }

    fn data_len(&self) -> u64 {
        self.total_len
    }

    fn segments(&self) -> &[TxSegment] {
        &self.segments
    }
}

/// Constructs a mana emulator backed by the loopback endpoint, then hooks a
/// mana driver up to it, puts the net_mana endpoint on top of that, and
/// ensures that packets can be sent and received.
#[async_test]
async fn test_endpoint_direct_dma(driver: DefaultDriver) {
    // 1 segment of 1138 bytes
    send_test_packet(
        driver.clone(),
        GuestDmaMode::DirectDma,
        1138,
        1,
        false, // LSO?
        None,  // Test config
        None,  // Default expected stats
    )
    .await;

    // 10 segments of 113 bytes each == 1130
    send_test_packet(
        driver.clone(),
        GuestDmaMode::DirectDma,
        1130,
        10,
        false, // LSO?
        None,  // Test config
        None,  // Default expected stats
    )
    .await;
}

#[async_test]
async fn test_endpoint_bounce_buffer(driver: DefaultDriver) {
    // 1 segment of 1138 bytes
    send_test_packet(
        driver,
        GuestDmaMode::BounceBuffer,
        1138,
        1,
        false, // LSO?
        None,  // Test config
        None,  // Default expected stats
    )
    .await;
}

#[async_test]
async fn test_segment_coalescing(driver: DefaultDriver) {
    // 34 segments of 60 bytes each == 2040
    send_test_packet(
        driver,
        GuestDmaMode::DirectDma,
        2040,
        34,
        false, // LSO?
        None,  // Test config
        None,  // Default expected stats
    )
    .await;
}

#[async_test]
async fn test_segment_coalescing_many(driver: DefaultDriver) {
    // 128 segments of 16 bytes each == 2048
    send_test_packet(
        driver.clone(),
        GuestDmaMode::DirectDma,
        2048,
        128,
        false, // LSO?
        None,  // Test config
        None,  // Default expected stats
    )
    .await;
}

#[async_test]
async fn test_packet_header_gt_head(driver: DefaultDriver) {
    let num_segments = 32;
    let packet_len = num_segments * (IPV4_HEADER_LENGTH - 10);
    send_test_packet(
        driver.clone(),
        GuestDmaMode::DirectDma,
        packet_len,
        num_segments,
        false, // LSO?
        None,  // Test config
        None,  // Default expected stats
    )
    .await;
}

#[async_test]
async fn test_lso_header_eq_head(driver: DefaultDriver) {
    // For the header (i.e. protocol) length to be equal to the head segment, make
    // the segment length equal to the protocol header length.
    let segment_len = IPV4_HEADER_LENGTH;
    let num_segments = MAX_GDMA_SGE_PER_TX_PACKET - 10;
    let packet_len = num_segments * segment_len;
    send_test_packet(
        driver.clone(),
        GuestDmaMode::DirectDma,
        packet_len,
        num_segments,
        true, // LSO?
        None, // Test config
        None, // Default expected stats
    )
    .await;

    // Caolescing test
    let num_segments = MAX_GDMA_SGE_PER_TX_PACKET + 1;
    let packet_len = num_segments * segment_len;
    send_test_packet(
        driver.clone(),
        GuestDmaMode::DirectDma,
        packet_len,
        num_segments,
        true, // LSO?
        None, // Test config
        None, // Default expected stats
    )
    .await;
}

#[async_test]
async fn test_lso_header_lt_head(driver: DefaultDriver) {
    // For the header (i.e. protocol) length to be less than the head segment, make
    // the segment length greater than the protocol header length to force the header
    // to fit in the first segment.
    let segment_len = IPV4_HEADER_LENGTH + 6;
    let num_segments = MAX_GDMA_SGE_PER_TX_PACKET - 10;
    let packet_len = num_segments * segment_len;
    send_test_packet(
        driver.clone(),
        GuestDmaMode::DirectDma,
        packet_len,
        num_segments,
        true, // LSO?
        None, // Test config
        None, // Default expected stats
    )
    .await;

    // Coalescing test
    let num_segments = MAX_GDMA_SGE_PER_TX_PACKET + 1;
    let packet_len = num_segments * segment_len;
    send_test_packet(
        driver.clone(),
        GuestDmaMode::DirectDma,
        packet_len,
        num_segments,
        true, // LSO?
        None, // Test config
        None, // Default expected stats
    )
    .await;
}

#[async_test]
async fn test_lso_header_gt_head(driver: DefaultDriver) {
    // For the header (i.e. protocol) length to be greater than the head segment, make
    // the segment length smaller than the protocol header length to force the header
    // to not fit in the first segment.
    let segment_len = IPV4_HEADER_LENGTH - 5;
    let num_segments = MAX_GDMA_SGE_PER_TX_PACKET - 10;
    let packet_len = num_segments * segment_len;
    send_test_packet(
        driver.clone(),
        GuestDmaMode::DirectDma,
        packet_len,
        num_segments,
        true, // LSO?
        None, // Test config
        None, // Default expected stats
    )
    .await;

    // Coalescing test
    let num_segments = MAX_GDMA_SGE_PER_TX_PACKET + 1;
    let packet_len = num_segments * segment_len;
    send_test_packet(
        driver.clone(),
        GuestDmaMode::DirectDma,
        packet_len,
        num_segments,
        true, // LSO?
        None, // Test config
        None, // Default expected stats
    )
    .await;
}

#[async_test]
async fn test_lso_split_header(driver: DefaultDriver) {
    // Invalid split header with header missing bytes (packet should get dropped).
    // Keep the total packet length less than the protocol header length.
    let segment_len = 1;
    let num_segments = IPV4_HEADER_LENGTH - 10;
    let packet_len = num_segments * segment_len;
    let expected_stats = Some(QueueStats {
        tx_packets: Counter::new(),
        rx_packets: Counter::new(),
        tx_errors: Counter::new(),
        rx_errors: Counter::new(),
        ..Default::default()
    });
    send_test_packet(
        driver.clone(),
        GuestDmaMode::DirectDma,
        packet_len,
        num_segments,
        true, // LSO?
        None, // Test config
        expected_stats,
    )
    .await;

    // Excessive splitting of the header, but keep the total packet length
    // the same as the protocol header length. The header should get coalesced
    // correctly back to one segment. With LSO, packet with one segment is
    // invalid and the expected result is that the packet gets dropped.
    let segment_len = 1;
    let num_segments = IPV4_HEADER_LENGTH;
    let packet_len = num_segments * segment_len;
    let expected_stats = Some(QueueStats {
        tx_packets: Counter::new(),
        rx_packets: Counter::new(),
        tx_errors: Counter::new(),
        rx_errors: Counter::new(),
        ..Default::default()
    });
    send_test_packet(
        driver.clone(),
        GuestDmaMode::DirectDma,
        packet_len,
        num_segments,
        true, // LSO?
        None, // Test config
        expected_stats,
    )
    .await;

    // Excessive splitting of the header, but total segment will be more than
    // one after coalescing. The packet should be accepted.
    let segment_len = 1;
    let num_segments = IPV4_HEADER_LENGTH + 10;
    let packet_len = num_segments * segment_len;
    send_test_packet(
        driver.clone(),
        GuestDmaMode::DirectDma,
        packet_len,
        num_segments,
        true, // LSO?
        None, // Test config
        None, // Default expected stats
    )
    .await;

    // Split headers such that the last header has both header and payload bytes.
    // i.e. The header should not evenly split into segments.
    let segment_len = 5;
    assert!(!IPV4_HEADER_LENGTH.is_multiple_of(segment_len));
    let num_segments = IPV4_HEADER_LENGTH + 10;
    let packet_len = num_segments * segment_len;
    send_test_packet(
        driver.clone(),
        GuestDmaMode::DirectDma,
        packet_len,
        num_segments,
        true, // LSO?
        None, // Test config
        None, // Default expected stats
    )
    .await;
}

#[async_test]
async fn test_lso_segment_coalescing_only_header(driver: DefaultDriver) {
    let segment_len = IPV4_HEADER_LENGTH;
    let num_segments = 1;
    let packet_len = num_segments * segment_len;
    // An LSO packet without any payload is considered bad packet and should be dropped.
    let expected_stats = Some(QueueStats {
        tx_packets: Counter::new(),
        rx_packets: Counter::new(),
        tx_errors: Counter::new(),
        rx_errors: Counter::new(),
        ..Default::default()
    });
    send_test_packet(
        driver.clone(),
        GuestDmaMode::DirectDma,
        packet_len,
        num_segments,
        true, // LSO?
        None, // Test config
        expected_stats,
    )
    .await;

    // Allow LSO with only header segment for test coverage and check that it
    // results in error stats incremented.
    let mut expected_stats = Some(QueueStats {
        tx_packets: Counter::new(),
        rx_packets: Counter::new(),
        tx_errors: Counter::new(),
        rx_errors: Counter::new(),
        ..Default::default()
    });

    expected_stats.as_mut().unwrap().tx_errors.add(1);
    let test_config = Some(ManaTestConfiguration {
        allow_lso_pkt_with_one_sge: true,
    });
    send_test_packet(
        driver.clone(),
        GuestDmaMode::DirectDma,
        packet_len,
        num_segments,
        true, // LSO?
        test_config,
        expected_stats,
    )
    .await;
}

// Tests for multiple packets in a single Tx call.
#[async_test]
async fn test_multi_packet(driver: DefaultDriver) {
    let mut num_packets = 0;
    let mut pkt_builder = TxPacketBuilder::new();
    let packet_len = 550;
    let num_segments = 1;
    let enable_lso = false;
    build_tx_segments(packet_len, num_segments, enable_lso, &mut pkt_builder);
    num_packets += 1;

    // Coalescing
    let packet_len = 2040;
    let num_segments = MAX_GDMA_SGE_PER_TX_PACKET + 3;
    build_tx_segments(packet_len, num_segments, enable_lso, &mut pkt_builder);
    num_packets += 1;

    // Split headers
    let segment_len = 1;
    let num_segments = IPV4_HEADER_LENGTH - 10;
    let packet_len = num_segments * segment_len;
    build_tx_segments(packet_len, num_segments, enable_lso, &mut pkt_builder);
    num_packets += 1;

    let packet_len = 650;
    let num_segments = 10;
    build_tx_segments(packet_len, num_segments, enable_lso, &mut pkt_builder);
    num_packets += 1;

    let mut expected_stats = QueueStats {
        ..Default::default()
    };
    expected_stats.tx_packets.add(num_packets);
    expected_stats.rx_packets.add(num_packets);

    send_test_packet_multi(
        driver.clone(),
        GuestDmaMode::DirectDma,
        &mut pkt_builder,
        None,                 // Test config
        Some(expected_stats), // Default expected stats
    )
    .await;
}

// Tests for multiple LSO packets in a single Tx call.
#[async_test]
async fn test_multi_lso_packet(driver: DefaultDriver) {
    let mut num_packets = 0;
    let enable_lso = true;
    let mut pkt_builder = TxPacketBuilder::new();
    // Header equals head segment.
    let segment_len = IPV4_HEADER_LENGTH;
    let num_segments = MAX_GDMA_SGE_PER_TX_PACKET - 10;
    let packet_len = segment_len * num_segments;
    build_tx_segments(packet_len, num_segments, enable_lso, &mut pkt_builder);
    num_packets += 1;

    // Coalescing
    let num_segments = MAX_GDMA_SGE_PER_TX_PACKET + 1;
    let packet_len = num_segments * segment_len;
    build_tx_segments(packet_len, num_segments, enable_lso, &mut pkt_builder);
    num_packets += 1;

    // Excessive splitting of split headers
    let segment_len = 1;
    let num_segments = IPV4_HEADER_LENGTH + 10;
    let packet_len = num_segments * segment_len;
    build_tx_segments(packet_len, num_segments, enable_lso, &mut pkt_builder);
    num_packets += 1;

    // Header greater than head segment.
    let segment_len = IPV4_HEADER_LENGTH - 5;
    let num_segments = MAX_GDMA_SGE_PER_TX_PACKET - 10;
    let packet_len = num_segments * segment_len;
    build_tx_segments(packet_len, num_segments, enable_lso, &mut pkt_builder);
    num_packets += 1;

    let mut expected_stats = QueueStats {
        ..Default::default()
    };
    expected_stats.tx_packets.add(num_packets);
    expected_stats.rx_packets.add(num_packets);

    send_test_packet_multi(
        driver.clone(),
        GuestDmaMode::DirectDma,
        &mut pkt_builder,
        None,                 // Test config
        Some(expected_stats), // Default expected stats
    )
    .await;
}

// Tests for multiple mixed (LSO and non-LSO) packets in a single Tx call.
#[async_test]
async fn test_multi_mixed_packet(driver: DefaultDriver) {
    let mut num_packets = 0;
    let mut pkt_builder = TxPacketBuilder::new();

    // Simple non-LSO packet
    let packet_len = 550;
    let num_segments = 1;
    build_tx_segments(packet_len, num_segments, false, &mut pkt_builder);
    num_packets += 1;

    // Excessive splitting of split headers for LSO packet
    let segment_len = 1;
    let num_segments = IPV4_HEADER_LENGTH + 10;
    let packet_len = num_segments * segment_len;
    build_tx_segments(packet_len, num_segments, true, &mut pkt_builder);
    num_packets += 1;

    // Coalescing for non-LSO packet
    let packet_len = 2040;
    let num_segments = MAX_GDMA_SGE_PER_TX_PACKET + 3;
    build_tx_segments(packet_len, num_segments, false, &mut pkt_builder);
    num_packets += 1;

    // Finish with a LSO packet.
    let segment_len = IPV4_HEADER_LENGTH - 5;
    let num_segments = MAX_GDMA_SGE_PER_TX_PACKET - 10;
    let packet_len = num_segments * segment_len;
    build_tx_segments(packet_len, num_segments, true, &mut pkt_builder);
    num_packets += 1;

    let mut expected_stats = QueueStats {
        ..Default::default()
    };
    expected_stats.tx_packets.add(num_packets);
    expected_stats.rx_packets.add(num_packets);

    send_test_packet_multi(
        driver.clone(),
        GuestDmaMode::DirectDma,
        &mut pkt_builder,
        None,                 // Test config
        Some(expected_stats), // Default expected stats
    )
    .await;
}

#[async_test]
async fn test_vport_with_query_filter_state(driver: DefaultDriver) {
    let pages = 512; // 2MB
    let mem = DeviceTestMemory::new(pages, false, "test_vport_with_query_filter_state");
    let mut msi_set = MsiInterruptSet::new();
    let device = gdma::GdmaDevice::new(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        mem.guest_memory(),
        &mut msi_set,
        vec![VportConfig {
            mac_address: [1, 2, 3, 4, 5, 6].into(),
            endpoint: Box::new(LoopbackEndpoint::new()),
        }],
        &mut ExternallyManagedMmioIntercepts,
    );
    let dma_client = mem.dma_client();
    let device = EmulatedDevice::new(device, msi_set, dma_client);
    let cap_flags1 = gdma_defs::bnic::BasicNicDriverFlags::new().with_query_filter_state(1);
    let dev_config = ManaQueryDeviceCfgResp {
        pf_cap_flags1: cap_flags1,
        pf_cap_flags2: 0,
        pf_cap_flags3: 0,
        pf_cap_flags4: 0,
        max_num_vports: 1,
        reserved: 0,
        max_num_eqs: 64,
    };
    let thing = ManaDevice::new(&driver, device, 1, 1, None).await.unwrap();
    let _ = thing.new_vport(0, None, &dev_config).await.unwrap();
}

#[async_test]
async fn test_rx_error_handling(driver: DefaultDriver) {
    // The GDMA BNic emulator provides a test hook: an RX completion where
    // metadata length equals RX_ERROR_TRIGGER_PACKET_LEN returns CQE_RX_ERR_DISABLED_QUEUE.
    let expected_num_tx_packets = 1; // One packet sent
    let expected_num_rx_packets = 0; // But RX fails due to the error hook
    let num_segments = 1;
    let packet_len = gdma_defs::test_hooks::RX_ERROR_TRIGGER_PACKET_LEN;

    let mut pkt_builder = TxPacketBuilder::new();
    build_tx_segments(packet_len, num_segments, false, &mut pkt_builder);

    let stats = test_endpoint(
        driver,
        GuestDmaMode::DirectDma,
        &pkt_builder,
        expected_num_tx_packets,
        expected_num_rx_packets,
        ManaTestConfiguration::default(),
    )
    .await;

    assert_eq!(stats.rx_errors.get(), 1, "rx_errors increase");
    assert_eq!(stats.rx_packets.get(), 0, "rx_packets stay the same");
    assert_eq!(stats.tx_packets.get(), 1, "tx_packets increases");
}

async fn send_test_packet(
    driver: DefaultDriver,
    dma_mode: GuestDmaMode,
    packet_len: usize,
    num_segments: usize,
    enable_lso: bool,
    test_config: Option<ManaTestConfiguration>,
    expected_stats: Option<QueueStats>,
) {
    let mut pkt_builder = TxPacketBuilder::new();
    build_tx_segments(packet_len, num_segments, enable_lso, &mut pkt_builder);
    send_test_packet_multi(
        driver,
        dma_mode,
        &mut pkt_builder,
        test_config,
        expected_stats,
    )
    .await;
}

async fn send_test_packet_multi(
    driver: DefaultDriver,
    dma_mode: GuestDmaMode,
    pkt_builder: &mut TxPacketBuilder,
    test_config: Option<ManaTestConfiguration>,
    expected_stats: Option<QueueStats>,
) {
    let test_config = test_config.unwrap_or_default();
    let expected_stats = expected_stats.unwrap_or_else(|| {
        let mut tx_packets = Counter::new();
        tx_packets.add(1);
        let mut rx_packets = Counter::new();
        rx_packets.add(1);
        QueueStats {
            tx_packets,
            rx_packets,
            tx_errors: Counter::new(),
            rx_errors: Counter::new(),
            ..Default::default()
        }
    });

    let stats = test_endpoint(
        driver,
        dma_mode,
        pkt_builder,
        expected_stats.tx_packets.get() as usize,
        expected_stats.rx_packets.get() as usize,
        test_config,
    )
    .await;

    assert_eq!(
        stats.tx_packets.get(),
        expected_stats.tx_packets.get(),
        "tx_packets mismatch"
    );
    assert_eq!(
        stats.rx_packets.get(),
        expected_stats.rx_packets.get(),
        "rx_packets mismatch"
    );
    assert_eq!(
        stats.tx_errors.get(),
        expected_stats.tx_errors.get(),
        "tx_errors mismatch"
    );
    assert_eq!(
        stats.rx_errors.get(),
        expected_stats.rx_errors.get(),
        "rx_errors mismatch"
    );
}

fn build_tx_segments(
    packet_len: usize,
    num_segments: usize,
    enable_lso: bool,
    pkt_builder: &mut TxPacketBuilder,
) {
    // Packet length must be divisible by number of segments.
    assert_eq!(packet_len % num_segments, 0);
    let tx_id = 1;
    let segment_len = packet_len / num_segments;
    let mut tx_metadata = net_backend::TxMetadata {
        id: TxId(tx_id),
        segment_count: num_segments as u8,
        len: packet_len as u32,
        l2_len: 14,                 // Ethernet header
        l3_len: 20,                 // IPv4 header
        l4_len: 20,                 // TCP header
        max_tcp_segment_size: 1460, // Typical MSS for Ethernet
        ..Default::default()
    };

    tx_metadata.flags.set_offload_tcp_segmentation(enable_lso);

    assert_eq!(
        tx_metadata.l2_len as usize + tx_metadata.l3_len as usize + tx_metadata.l4_len as usize,
        IPV4_HEADER_LENGTH
    );
    assert_eq!(packet_len % num_segments, 0);

    let mut gpa = pkt_builder.data_len();
    pkt_builder.push(TxSegment {
        ty: net_backend::TxSegmentType::Head(tx_metadata.clone()),
        gpa,
        len: segment_len as u32,
    });

    for _ in 0..(num_segments - 1) {
        gpa += segment_len as u64;
        pkt_builder.push(TxSegment {
            ty: net_backend::TxSegmentType::Tail,
            gpa,
            len: segment_len as u32,
        });
    }
}

async fn test_endpoint(
    driver: DefaultDriver,
    dma_mode: GuestDmaMode,
    pkt_builder: &TxPacketBuilder,
    expected_num_send_packets: usize,
    expected_num_received_packets: usize,
    test_configuration: ManaTestConfiguration,
) -> QueueStats {
    let pages = 256; // 1MB
    let allow_dma = dma_mode == GuestDmaMode::DirectDma;
    let mem: DeviceTestMemory = DeviceTestMemory::new(pages * 2, allow_dma, "test_endpoint");
    let payload_mem = mem.payload_mem();
    let data_to_send = pkt_builder.packet_data();
    let tx_segments = pkt_builder.segments();

    let mut msi_set = MsiInterruptSet::new();
    let device = gdma::GdmaDevice::new(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        mem.guest_memory(),
        &mut msi_set,
        vec![VportConfig {
            mac_address: [1, 2, 3, 4, 5, 6].into(),
            endpoint: Box::new(LoopbackEndpoint::new()),
        }],
        &mut ExternallyManagedMmioIntercepts,
    );
    let device = EmulatedDevice::new(device, msi_set, mem.dma_client());
    let dev_config = ManaQueryDeviceCfgResp {
        pf_cap_flags1: 0.into(),
        pf_cap_flags2: 0,
        pf_cap_flags3: 0,
        pf_cap_flags4: 0,
        max_num_vports: 1,
        reserved: 0,
        max_num_eqs: 64,
    };
    let thing = ManaDevice::new(&driver, device, 1, 1, None).await.unwrap();
    let vport = thing.new_vport(0, None, &dev_config).await.unwrap();
    let mut endpoint = ManaEndpoint::new(driver.clone(), vport, dma_mode).await;
    endpoint.set_test_configuration(test_configuration);
    let mut queues = Vec::new();
    let pool = net_backend::tests::Bufs::new(payload_mem.clone());
    endpoint
        .get_queues(
            vec![QueueConfig {
                pool: Box::new(pool.clone()),
                initial_rx: &(1..128).map(RxId).collect::<Vec<_>>(),
                driver: Box::new(driver.clone()),
            }],
            None,
            &mut queues,
        )
        .await
        .unwrap();

    payload_mem.write_at(0, &data_to_send).unwrap();

    queues[0].tx_avail(tx_segments).unwrap();

    // Poll for completion
    // Keep at least couple of elements in the Rx and Tx done vectors to
    // allow for zero packet tests.
    let mut rx_packets = (0..expected_num_received_packets.max(2))
        .map(|i| RxId(i as u32))
        .collect::<Vec<_>>();
    let mut rx_packets_n = 0;
    let mut tx_done = vec![TxId(0); expected_num_send_packets.max(2)];
    let mut tx_done_n = 0;

    // Wait until we have received the expected number of RX packets,
    // or until we have completed the expected TX packets (for error cases where RX fails).
    let done = |rx_n: usize, tx_n: usize| -> bool {
        if expected_num_received_packets > 0 {
            // Normal case: wait for RX packets
            rx_n >= expected_num_received_packets
        } else if expected_num_send_packets > 0 {
            // Error case: RX is expected to fail, wait for TX completion instead
            tx_n >= expected_num_send_packets
        } else {
            // Both are 0: nothing to wait for, done
            true
        }
    };

    loop {
        let mut context = CancelContext::new().with_timeout(Duration::from_secs(1));
        match context
            .until_cancelled(poll_fn(|cx| queues[0].poll_ready(cx)))
            .await
        {
            Err(CancelReason::DeadlineExceeded) => break,
            Err(e) => {
                tracing::error!(error = ?e, "Failed to poll queue ready");
                break;
            }
            _ => {}
        }
        rx_packets_n += queues[0].rx_poll(&mut rx_packets[rx_packets_n..]).unwrap();
        // GDMA Errors generate a TryReturn error, ignored here.
        tx_done_n += queues[0].tx_poll(&mut tx_done[tx_done_n..]).unwrap_or(0);
        if done(rx_packets_n, tx_done_n) {
            break;
        }
    }
    assert_eq!(rx_packets_n, expected_num_received_packets);
    assert_eq!(tx_done_n, expected_num_send_packets);

    if expected_num_received_packets == 0 {
        // If no packets were received, exit.
        let stats = get_queue_stats(queues[0].queue_stats());
        drop(queues);
        endpoint.stop().await;
        return stats;
    }

    // GDMA emulator always returns TxId(1) for completed packets.
    for done in tx_done.iter().take(expected_num_send_packets) {
        assert_eq!(done.0, 1);
    }

    // Check rx
    let mut offset = 0;
    for (i, rx_id) in rx_packets
        .iter()
        .enumerate()
        .take(expected_num_received_packets)
    {
        let this_pkt_len = pkt_builder.pkt_len[i] as usize;
        let mut received_data = vec![0; this_pkt_len];
        assert_eq!(rx_id.0, (i + 1) as u32);
        let buffer_size = pool.capacity(*rx_id) as u64;
        payload_mem
            .read_at(buffer_size * rx_id.0 as u64, &mut received_data)
            .unwrap();
        assert_eq!(received_data.len(), this_pkt_len);
        assert_eq!(
            &received_data,
            &data_to_send[offset..offset + this_pkt_len],
            "{:?}",
            rx_id
        );
        offset += this_pkt_len;
    }

    let stats = get_queue_stats(queues[0].queue_stats());
    drop(queues);
    endpoint.stop().await;
    stats
}

fn get_queue_stats(queue_stats: Option<&dyn net_backend::BackendQueueStats>) -> QueueStats {
    let queue_stats = queue_stats.unwrap();
    QueueStats {
        rx_errors: queue_stats.rx_errors(),
        tx_errors: queue_stats.tx_errors(),
        rx_packets: queue_stats.rx_packets(),
        tx_packets: queue_stats.tx_packets(),
        ..Default::default()
    }
}
