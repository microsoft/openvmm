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

/// Constructs a mana emulator backed by the loopback endpoint, then hooks a
/// mana driver up to it, puts the net_mana endpoint on top of that, and
/// ensures that packets can be sent and received.
#[async_test]
async fn test_endpoint_direct_dma(driver: DefaultDriver) {
    send_test_packet(driver, GuestDmaMode::DirectDma, 1138, 1).await;
}

#[async_test]
async fn test_endpoint_bounce_buffer(driver: DefaultDriver) {
    send_test_packet(driver, GuestDmaMode::BounceBuffer, 1138, 1).await;
}

#[async_test]
async fn test_segment_coalescing(driver: DefaultDriver) {
    // 34 segments of 60 bytes each == 2040
    send_test_packet(driver, GuestDmaMode::DirectDma, 2040, 34).await;
}

#[async_test]
async fn test_segment_coalescing_many(driver: DefaultDriver) {
    // 128 segments of 16 bytes each == 2048
    send_test_packet(driver, GuestDmaMode::DirectDma, 2048, 128).await;
}

#[async_test]
async fn test_lso(driver: DefaultDriver) {
    // Num segments should be within the hardware limit of 31 segments.
    let num_segments = 30;
    let mut metadata = net_backend::TxMetadata {
        id: TxId(1),
        segment_count: num_segments as u8,
        ..Default::default()
    };

    metadata.flags.set_offload_tcp_segmentation(true);
    metadata.l2_len = 14; // Ethernet header
    metadata.l3_len = 20; // IPv4 header
    metadata.l4_len = 20; // TCP header
    metadata.max_tcp_segment_size = 1460; // Typical MSS for Ethernet
    metadata.flags.set_is_ipv4(true);
    let header_length =
        (metadata.l2_len as u16 + metadata.l3_len + metadata.l4_len as u16) as usize;
    let packet_len: usize = num_segments * header_length;
    let stats = test_endpoint_lso(
        driver,
        packet_len,
        num_segments,
        metadata,
        1,
        ManaTestConfiguration::default(),
    )
    .await;

    assert_eq!(stats.tx_packets.get(), 1, "tx_packets increase");
    assert_eq!(stats.rx_packets.get(), 1, "rx_packets increase");
    assert_eq!(stats.tx_errors.get(), 0, "tx_errors remain the same");
    assert_eq!(stats.rx_errors.get(), 0, "rx_errors remain the same");
}

#[async_test]
async fn test_lso_partial_bytes(driver: DefaultDriver) {
    // Num segments should be within the hardware limit of 31 segments.
    let num_segments = 30;
    let mut metadata = net_backend::TxMetadata {
        id: TxId(1),
        segment_count: num_segments as u8,
        ..Default::default()
    };

    metadata.flags.set_offload_tcp_segmentation(true);
    metadata.l2_len = 14; // Ethernet header
    metadata.l3_len = 20; // IPv4 header
    metadata.l4_len = 20; // TCP header
    metadata.max_tcp_segment_size = 1460; // Typical MSS for Ethernet
    metadata.flags.set_is_ipv4(true);
    let header_length =
        (metadata.l2_len as u16 + metadata.l3_len + metadata.l4_len as u16) as usize;
    // Add a few bytes to header to mimic the head segment being larger than the header length.
    let packet_len: usize = num_segments * (header_length + 2);
    let stats = test_endpoint_lso(
        driver,
        packet_len,
        num_segments,
        metadata,
        1,
        ManaTestConfiguration::default(),
    )
    .await;

    assert_eq!(stats.tx_packets.get(), 1, "tx_packets increase");
    assert_eq!(stats.rx_packets.get(), 1, "rx_packets increase");
    assert_eq!(stats.tx_errors.get(), 0, "tx_errors remain the same");
    assert_eq!(stats.rx_errors.get(), 0, "rx_errors remain the same");
}

#[async_test]
async fn test_lso_segment_coalescing(driver: DefaultDriver) {
    // Num segments should exceed the hardware limit of 31 segments to force coalescing.
    let num_segments = 36;
    let mut metadata = net_backend::TxMetadata {
        id: TxId(1),
        segment_count: num_segments as u8,
        ..Default::default()
    };

    metadata.flags.set_offload_tcp_segmentation(true);
    metadata.l2_len = 14; // Ethernet header
    metadata.l3_len = 20; // IPv4 header
    metadata.l4_len = 20; // TCP header
    metadata.max_tcp_segment_size = 1460; // Typical MSS for Ethernet
    metadata.flags.set_is_ipv4(true);
    let header_length =
        (metadata.l2_len as u16 + metadata.l3_len + metadata.l4_len as u16) as usize;
    let packet_len: usize = num_segments * header_length;
    let stats = test_endpoint_lso(
        driver,
        packet_len,
        num_segments,
        metadata,
        1,
        ManaTestConfiguration::default(),
    )
    .await;

    assert_eq!(stats.tx_packets.get(), 1, "tx_packets increase");
    assert_eq!(stats.rx_packets.get(), 1, "rx_packets increase");
    assert_eq!(stats.tx_errors.get(), 0, "tx_errors remain the same");
    assert_eq!(stats.rx_errors.get(), 0, "rx_errors remain the same");
}

#[async_test]
async fn test_lso_segment_coalescing_partial_bytes_in_header(driver: DefaultDriver) {
    // Num segments should exceed the hardware limit of 31 segments to force coalescing.
    let num_segments = 36;
    let mut metadata = net_backend::TxMetadata {
        id: TxId(1),
        segment_count: num_segments as u8,
        ..Default::default()
    };

    metadata.flags.set_offload_tcp_segmentation(true);
    metadata.l2_len = 14; // Ethernet header
    metadata.l3_len = 20; // IPv4 header
    metadata.l4_len = 20; // TCP header
    metadata.max_tcp_segment_size = 1460; // Typical MSS for Ethernet
    metadata.flags.set_is_ipv4(true);
    let header_length =
        (metadata.l2_len as u16 + metadata.l3_len + metadata.l4_len as u16) as usize;
    // Add a few bytes to header to mimic the head segment being larger than the header length.
    let packet_len: usize = num_segments * (header_length + 2);
    let stats = test_endpoint_lso(
        driver,
        packet_len,
        num_segments,
        metadata,
        1,
        ManaTestConfiguration::default(),
    )
    .await;

    assert_eq!(stats.tx_packets.get(), 1, "tx_packets increase");
    assert_eq!(stats.rx_packets.get(), 1, "rx_packets increase");
    assert_eq!(stats.tx_errors.get(), 0, "tx_errors remain the same");
    assert_eq!(stats.rx_errors.get(), 0, "rx_errors remain the same");
}

#[async_test]
async fn test_lso_segment_coalescing_only_header(driver: DefaultDriver) {
    let num_segments = 1;
    let mut metadata = net_backend::TxMetadata {
        id: TxId(1),
        segment_count: num_segments as u8,
        ..Default::default()
    };

    metadata.flags.set_offload_tcp_segmentation(true);
    metadata.l2_len = 14; // Ethernet header
    metadata.l3_len = 20; // IPv4 header
    metadata.l4_len = 20; // TCP header
    metadata.max_tcp_segment_size = 1460; // Typical MSS for Ethernet
    metadata.flags.set_is_ipv4(true);
    let header_length =
        (metadata.l2_len as u16 + metadata.l3_len + metadata.l4_len as u16) as usize;
    let packet_len: usize = num_segments * header_length;

    // An LSO packet without any payload is considered bad packet and should be dropped.
    let stats = test_endpoint_lso(
        driver.clone(),
        packet_len,
        num_segments,
        metadata.clone(),
        0,
        ManaTestConfiguration::default(),
    )
    .await;

    assert_eq!(stats.tx_packets.get(), 0, "tx_packets remain the same");
    assert_eq!(stats.rx_packets.get(), 0, "rx_packets remain the same");
    assert_eq!(stats.tx_errors.get(), 0, "tx_errors remain the same");
    assert_eq!(stats.rx_errors.get(), 0, "rx_errors remain the same");

    // Allow LSO with only header segment for test coverage and check that it
    // results in error stats incremented.
    let stats = test_endpoint_lso(
        driver.clone(),
        packet_len,
        num_segments,
        metadata.clone(),
        0,
        ManaTestConfiguration {
            allow_lso_pkt_with_one_sge: true,
        },
    )
    .await;

    assert_eq!(stats.tx_packets.get(), 0, "tx_packets remain the same");
    assert_eq!(stats.rx_packets.get(), 0, "rx_packets remain the same");
    assert_eq!(stats.tx_errors.get(), 1, "tx_errors increase");
    assert_eq!(stats.rx_errors.get(), 0, "rx_errors remain the same");
}

macro_rules! lso_split_headers {
    // This macro generates segments for LSO packets with split headers.
    // It creates a head segment with reduced header length and the subsequent segment
    // that contain the remaining header (and possibly the payload).
    // The number of segments is specified by `$num_segments`.
    // The segments are pushed into the `$segments` vector.
    // If `$mix_hdr_with_payload` is true, the header and payload are mixed in
    // the second segment, otherwise the second segment will only contain the split header.
    // The macro returns the total packet length.
    // When `$mix_hdr_with_payload` is false, the number of segments should be greater than 2
    ($num_segments:ident, $segments:ident, $mix_hdr_with_payload:ident) => {{
        assert!(
            $mix_hdr_with_payload || $num_segments > 2,
            "When not mixing header with payload, num_segments should be greater than 2"
        );
        let mut metadata = net_backend::TxMetadata {
            id: TxId(1),
            segment_count: $num_segments,
            ..Default::default()
        };

        metadata.flags.set_offload_tcp_segmentation(true);
        metadata.l2_len = 14; // Ethernet header
        metadata.l3_len = 20; // IPv4 header
        metadata.l4_len = 20; // TCP header
        metadata.max_tcp_segment_size = 1460; // Typical MSS for Ethernet
        metadata.flags.set_is_ipv4(true);

        let header_length =
            (metadata.l2_len as u16 + metadata.l3_len + metadata.l4_len as u16) as usize;
        let packet_len: usize = $num_segments as usize * header_length;
        let segment_len = packet_len / $num_segments as usize;
        // Reduce the header length to force split headers.
        let header_bytes_remaining = 10;
        let reduced_header_len: u32 = (header_length - header_bytes_remaining) as u32;
        $segments.push(TxSegment {
            ty: net_backend::TxSegmentType::Head(metadata),
            gpa: 0,
            len: reduced_header_len as u32,
        });

        let mut gpa = reduced_header_len;
        let mut bytes_remaining = header_bytes_remaining;
        for j in 0..($num_segments - 1) {
            let this_segment_len = if j == 0 && !$mix_hdr_with_payload {
                let ret = bytes_remaining;
                bytes_remaining = segment_len;
                ret
            } else {
                let ret = segment_len + bytes_remaining;
                bytes_remaining = 0;
                ret
            };

            $segments.push(TxSegment {
                ty: net_backend::TxSegmentType::Tail,
                gpa: gpa as u64,
                len: this_segment_len as u32,
            });
            gpa += this_segment_len as u32;
        }
        packet_len
    }};
}

#[async_test]
async fn test_lso_split_headers(driver: DefaultDriver) {
    let num_segments = 2;
    let mut tx_segments = Vec::new();
    tracing::trace!("LSO split headers without coalescing (header mixed with payload)");
    let packet_len = lso_split_headers!(num_segments, tx_segments, true);
    let data_to_send = (0..packet_len).map(|v| v as u8).collect::<Vec<u8>>();
    let expected_num_received_packets = 1;
    let stats = test_endpoint(
        driver.clone(),
        GuestDmaMode::DirectDma,
        packet_len,
        tx_segments,
        data_to_send,
        expected_num_received_packets,
        ManaTestConfiguration::default(),
    )
    .await;

    assert_eq!(
        stats.tx_packets.get(),
        expected_num_received_packets as u64,
        "tx_packets increase"
    );
    assert_eq!(
        stats.rx_packets.get(),
        expected_num_received_packets as u64,
        "rx_packets increase"
    );
    assert_eq!(stats.tx_errors.get(), 0, "tx_errors remain the same");
    assert_eq!(stats.rx_errors.get(), 0, "rx_errors remain the same");

    let num_segments = 3;
    let mut tx_segments = Vec::new();
    tracing::trace!("LSO split headers without coalescing (header not mixed with payload)");
    let packet_len = lso_split_headers!(num_segments, tx_segments, false);
    let expected_num_received_packets = 1;
    let data_to_send = (0..packet_len).map(|v| v as u8).collect::<Vec<u8>>();
    let stats = test_endpoint(
        driver.clone(),
        GuestDmaMode::DirectDma,
        packet_len,
        tx_segments,
        data_to_send,
        expected_num_received_packets,
        ManaTestConfiguration::default(),
    )
    .await;

    assert_eq!(
        stats.tx_packets.get(),
        expected_num_received_packets as u64,
        "tx_packets increase"
    );
    assert_eq!(
        stats.rx_packets.get(),
        expected_num_received_packets as u64,
        "rx_packets increase"
    );
    assert_eq!(stats.tx_errors.get(), 0, "tx_errors remain the same");
    assert_eq!(stats.rx_errors.get(), 0, "rx_errors remain the same");

    // Test for split headers with coalescing. i.e. segment
    // count has to exceed the hardware limit of 31
    let num_segments = 33;
    let mut tx_segments = Vec::new();
    tracing::trace!("LSO split headers with coalescing (header mixed with payload)");
    let packet_len = lso_split_headers!(num_segments, tx_segments, true);
    let data_to_send = (0..packet_len).map(|v| v as u8).collect::<Vec<u8>>();
    let stats = test_endpoint(
        driver.clone(),
        GuestDmaMode::DirectDma,
        packet_len,
        tx_segments,
        data_to_send,
        expected_num_received_packets,
        ManaTestConfiguration::default(),
    )
    .await;

    assert_eq!(
        stats.tx_packets.get(),
        expected_num_received_packets as u64,
        "tx_packets increase"
    );
    assert_eq!(
        stats.rx_packets.get(),
        expected_num_received_packets as u64,
        "rx_packets increase"
    );
    assert_eq!(stats.tx_errors.get(), 0, "tx_errors remain the same");
    assert_eq!(stats.rx_errors.get(), 0, "rx_errors remain the same");

    let num_segments = 33;
    let mut tx_segments = Vec::new();
    tracing::trace!("LSO split headers with coalescing (header not mixed with payload)");
    let packet_len = lso_split_headers!(num_segments, tx_segments, false);
    let data_to_send = (0..packet_len).map(|v| v as u8).collect::<Vec<u8>>();

    let stats = test_endpoint(
        driver.clone(),
        GuestDmaMode::DirectDma,
        packet_len,
        tx_segments,
        data_to_send,
        expected_num_received_packets,
        ManaTestConfiguration::default(),
    )
    .await;

    assert_eq!(
        stats.tx_packets.get(),
        expected_num_received_packets as u64,
        "tx_packets increase"
    );
    assert_eq!(
        stats.rx_packets.get(),
        expected_num_received_packets as u64,
        "rx_packets increase"
    );
    assert_eq!(stats.tx_errors.get(), 0, "tx_errors remain the same");
    assert_eq!(stats.rx_errors.get(), 0, "rx_errors remain the same");
}

async fn send_test_packet(
    driver: DefaultDriver,
    dma_mode: GuestDmaMode,
    packet_len: usize,
    num_segments: usize,
) {
    let tx_id = 1;
    let tx_metadata = net_backend::TxMetadata {
        id: TxId(tx_id),
        segment_count: num_segments as u8,
        len: packet_len as u32,
        ..Default::default()
    };
    let expected_num_received_packets = 1;
    let (data_to_send, tx_segments) =
        build_tx_segments(packet_len, num_segments, tx_metadata.clone());

    let stats = test_endpoint(
        driver,
        dma_mode,
        packet_len,
        tx_segments,
        data_to_send,
        expected_num_received_packets,
        ManaTestConfiguration::default(),
    )
    .await;

    assert_eq!(
        stats.tx_packets.get(),
        expected_num_received_packets as u64,
        "tx_packets increase"
    );
    assert_eq!(
        stats.rx_packets.get(),
        expected_num_received_packets as u64,
        "rx_packets increase"
    );
    assert_eq!(stats.tx_errors.get(), 0, "tx_errors remain the same");
    assert_eq!(stats.rx_errors.get(), 0, "rx_errors remain the same");
}

fn build_tx_segments(
    packet_len: usize,
    num_segments: usize,
    tx_metadata: net_backend::TxMetadata,
) -> (Vec<u8>, Vec<TxSegment>) {
    let data_to_send = (0..packet_len).map(|v| v as u8).collect::<Vec<u8>>();

    let mut tx_segments = Vec::new();
    let segment_len = packet_len / num_segments;
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

async fn test_endpoint_lso(
    driver: DefaultDriver,
    packet_len: usize,
    num_segments: usize,
    mut metadata: net_backend::TxMetadata,
    expected_num_recvd_packets: usize,
    test_configuration: ManaTestConfiguration,
) -> QueueStats {
    let mut tx_segments = Vec::new();
    let segment_len = packet_len / num_segments;
    assert_eq!(packet_len % num_segments, 0);
    metadata.len = packet_len as u32;
    let data_to_send = (0..packet_len).map(|v| v as u8).collect::<Vec<u8>>();

    metadata.flags.set_offload_tcp_segmentation(true);
    tx_segments.push(TxSegment {
        ty: net_backend::TxSegmentType::Head(metadata),
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

    test_endpoint(
        driver,
        GuestDmaMode::DirectDma,
        packet_len,
        tx_segments,
        data_to_send,
        expected_num_recvd_packets,
        test_configuration,
    )
    .await
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
    let thing = ManaDevice::new(&driver, device, 1, 1).await.unwrap();
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
    let thing = ManaDevice::new(&driver, device, 1, 1).await.unwrap();
    let _ = thing.new_vport(0, None, &dev_config).await.unwrap();
}

#[async_test]
async fn test_valid_packet(driver: DefaultDriver) {
    let tx_id = 1;
    let expected_num_received_packets = 1;
    let segment_count = 1;
    let packet_len = 1138;
    let metadata = net_backend::TxMetadata {
        id: TxId(tx_id),
        segment_count: segment_count as u8,
        len: packet_len as u32,
        ..Default::default()
    };

    let (data_to_send, tx_segments) = build_tx_segments(packet_len, segment_count, metadata);

    let stats = test_endpoint(
        driver,
        GuestDmaMode::DirectDma,
        packet_len,
        tx_segments,
        data_to_send,
        expected_num_received_packets,
        ManaTestConfiguration::default(),
    )
    .await;

    assert_eq!(stats.tx_packets.get(), 1, "tx_packets increase");
    assert_eq!(stats.rx_packets.get(), 1, "rx_packets increase");
    assert_eq!(stats.tx_errors.get(), 0, "tx_errors remain the same");
    assert_eq!(stats.rx_errors.get(), 0, "rx_errors remain the same");
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
