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

async fn send_test_packet(
    driver: DefaultDriver,
    dma_mode: GuestDmaMode,
    packet_len: usize,
    num_segments: usize,
    enable_lso: bool,
    test_config: Option<ManaTestConfiguration>,
    expected_stats: Option<QueueStats>,
) {
    let (data_to_send, tx_segments) = build_tx_segments(packet_len, num_segments, enable_lso);

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
        packet_len,
        tx_segments,
        data_to_send,
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
) -> (Vec<u8>, Vec<TxSegment>) {
    // Packet length must be divisible by number of segments.
    assert_eq!(packet_len % num_segments, 0);
    let data_to_send = (0..packet_len).map(|v| v as u8).collect::<Vec<u8>>();
    let tx_id = 1;
    let mut tx_segments = Vec::new();
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
    assert_eq!(data_to_send.len(), packet_len);

    tx_segments.push(TxSegment {
        ty: net_backend::TxSegmentType::Head(tx_metadata.clone()),
        gpa: 0,
        len: segment_len as u32,
    });

    for j in 0..(num_segments - 1) {
        let gpa = (j + 1) * segment_len;
        tx_segments.push(TxSegment {
            ty: net_backend::TxSegmentType::Tail,
            gpa: gpa as u64,
            len: segment_len as u32,
        });
    }

    assert_eq!(tx_segments.len(), num_segments);
    (data_to_send, tx_segments)
}

async fn test_endpoint(
    driver: DefaultDriver,
    dma_mode: GuestDmaMode,
    packet_len: usize,
    tx_segments: Vec<TxSegment>,
    data_to_send: Vec<u8>,
    expected_num_received_packets: usize,
    test_configuration: ManaTestConfiguration,
) -> QueueStats {
    let tx_id = 1;
    let pages = 256; // 1MB
    let allow_dma = dma_mode == GuestDmaMode::DirectDma;
    let mem: DeviceTestMemory = DeviceTestMemory::new(pages * 2, allow_dma, "test_endpoint");
    let payload_mem = mem.payload_mem();

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
                pool: Box::new(pool),
                initial_rx: &(1..128).map(RxId).collect::<Vec<_>>(),
                driver: Box::new(driver.clone()),
            }],
            None,
            &mut queues,
        )
        .await
        .unwrap();

    payload_mem.write_at(0, &data_to_send).unwrap();

    queues[0].tx_avail(tx_segments.as_slice()).unwrap();

    // Poll for completion
    let mut rx_packets = [RxId(0); 2];
    let mut rx_packets_n = 0;
    let mut tx_done = [TxId(0); 2];
    let mut tx_done_n = 0;
    while rx_packets_n == 0 {
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
        if expected_num_received_packets == 0 {
            break;
        }
    }
    assert_eq!(rx_packets_n, expected_num_received_packets);

    if expected_num_received_packets == 0 {
        // If no packets were received, exit.
        let stats = get_queue_stats(queues[0].queue_stats());
        drop(queues);
        endpoint.stop().await;
        return stats;
    }

    // Check tx
    assert_eq!(tx_done_n, 1);
    assert_eq!(tx_done[0].0, tx_id);

    // Check rx
    assert_eq!(rx_packets[0].0, 1);
    let rx_id = rx_packets[0];

    let mut received_data = vec![0; packet_len];
    payload_mem
        .read_at(2048 * rx_id.0 as u64, &mut received_data)
        .unwrap();
    assert_eq!(received_data.len(), packet_len);
    assert_eq!(&received_data[..], data_to_send, "{:?}", rx_id);

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
