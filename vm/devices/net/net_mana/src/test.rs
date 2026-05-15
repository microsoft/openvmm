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
use net_backend::VlanMetadata;
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
const IPV4_VLAN_HEADER_LENGTH: usize = 58;
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

<<<<<<< HEAD
=======
/// Verifies that the link speed queried from the adapter via the full driver
/// stack is reported correctly through `dev_config().link_speed_bps()`,
/// `vport.link_speed_bps()`, and `endpoint.link_speed()`.
///
/// The emulated GDMA device returns `adapter_link_speed_mbps = 0`, so the
/// driver-stack path exercises the 200 Gbps fallback.
#[async_test]
async fn test_link_speed_default(driver: DefaultDriver) {
    // Verify that a non-zero adapter_link_speed_mbps is converted to bps
    // correctly, without going through the driver stack.
    let dev_config_nonzero = ManaQueryDeviceCfgResp {
        pf_cap_flags1: 0.into(),
        pf_cap_flags2: 0,
        pf_cap_flags3: 0,
        pf_cap_flags4: 0,
        max_num_vports: 1,
        reserved: 0,
        max_num_eqs: 64,
        adapter_mtu: 0,
        reserved2: 0,
        adapter_link_speed_mbps: 100 * 1000, // 100 Gbps
    };
    assert_eq!(
        dev_config_nonzero.link_speed_bps(),
        100 * 1000 * 1000 * 1000
    );

    // Now exercise the full driver stack. The emulated GDMA device returns
    // adapter_link_speed_mbps = 0, so the 200 Gbps fallback is expected
    // throughout.
    const FALLBACK_LINK_SPEED_BPS: u64 = 200 * 1000 * 1000 * 1000;

    let pages = 512; // 2MB
    let mem = DeviceTestMemory::new(pages, false, "test_link_speed_default");
    let msi_conn = MsiConnection::new(AssignedBusRange::new(), 0);
    let device = gdma::GdmaDevice::new(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        mem.guest_memory(),
        msi_conn.target(),
        vec![VportConfig {
            mac_address: [1, 2, 3, 4, 5, 6].into(),
            endpoint: Box::new(LoopbackEndpoint::new()),
        }],
        &mut ExternallyManagedMmioIntercepts,
    );
    let device = EmulatedDevice::new(device, msi_conn, mem.dma_client());

    let mana_device = ManaDevice::new(&driver, device, 1, 1, None).await.unwrap();

    // Verify the link speed as seen in the device config populated by
    // query_dev_config() during ManaDevice::new().
    assert_eq!(
        mana_device.dev_config().link_speed_bps(),
        FALLBACK_LINK_SPEED_BPS
    );

    let vport = mana_device
        .new_vport(
            0,
            None,
            &ManaQueryDeviceCfgResp {
                pf_cap_flags1: 0.into(),
                pf_cap_flags2: 0,
                pf_cap_flags3: 0,
                pf_cap_flags4: 0,
                max_num_vports: 1,
                reserved: 0,
                max_num_eqs: 64,
                adapter_mtu: 0,
                reserved2: 0,
                adapter_link_speed_mbps: 0,
            },
        )
        .await
        .unwrap();

    // The vport inherits the link speed from the ManaDevice (emulator value).
    assert_eq!(vport.link_speed_bps(), FALLBACK_LINK_SPEED_BPS);

    // Verify it is also surfaced correctly through the Endpoint trait.
    let mut endpoint = ManaEndpoint::new(driver.clone(), vport, GuestDmaMode::DirectDma).await;
    assert_eq!(endpoint.link_speed(), FALLBACK_LINK_SPEED_BPS);
    endpoint.stop().await;
}

/// Verifies that a link speed configured on the emulated GDMA device propagates
/// through the full net_mana driver stack: `dev_config().link_speed_bps()`,
/// `vport.link_speed_bps()`, and `endpoint.link_speed()`.
#[async_test]
async fn test_link_speed_expected(driver: DefaultDriver) {
    verify_link_speed_expected(driver, 400 * 1000).await; // 400 Gbps
}

async fn verify_link_speed_expected(driver: DefaultDriver, link_speed_mbps: u32) {
    let link_speed_bps = link_speed_mbps as u64 * 1000 * 1000;

    let pages = 512;
    let mem = DeviceTestMemory::new(pages, false, "test_link_speed_expected");
    let msi_conn = MsiConnection::new(AssignedBusRange::new(), 0);
    let device = gdma::GdmaDevice::new_with_config(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        mem.guest_memory(),
        msi_conn.target(),
        vec![VportConfig {
            mac_address: [1, 2, 3, 4, 5, 6].into(),
            endpoint: Box::new(LoopbackEndpoint::new()),
        }],
        &mut ExternallyManagedMmioIntercepts,
        gdma::BnicConfig {
            adapter_link_speed_mbps: link_speed_mbps,
        },
    );
    let device = EmulatedDevice::new(device, msi_conn, mem.dma_client());

    let thing = ManaDevice::new(&driver, device, 1, 1, None).await.unwrap();

    // Layer 1: dev_config stored on ManaDevice.
    assert_eq!(
        thing.dev_config().link_speed_bps(),
        link_speed_bps,
        "dev_config().link_speed_bps() should reflect the configured link speed"
    );

    let vport_dev_config = ManaQueryDeviceCfgResp {
        pf_cap_flags1: 0.into(),
        pf_cap_flags2: 0,
        pf_cap_flags3: 0,
        pf_cap_flags4: 0,
        max_num_vports: 1,
        reserved: 0,
        max_num_eqs: 64,
        adapter_mtu: 0,
        reserved2: 0,
        adapter_link_speed_mbps: 0,
    };
    let vport = thing.new_vport(0, None, &vport_dev_config).await.unwrap();

    // Layer 2: vport derives its link speed from the stored dev_config,
    // not from the per-call vport_dev_config.
    assert_eq!(
        vport.link_speed_bps(),
        link_speed_bps,
        "vport.link_speed_bps() should reflect the configured link speed"
    );

    // Layer 3: ManaEndpoint surfaces it via the Endpoint trait.
    let mut endpoint = ManaEndpoint::new(driver.clone(), vport, GuestDmaMode::DirectDma).await;
    assert_eq!(
        endpoint.link_speed(),
        link_speed_bps,
        "endpoint.link_speed() should reflect the configured link speed"
    );
    endpoint.stop().await;
}

#[async_test]
async fn test_rx_error_handling(driver: DefaultDriver) {
    // Send a packet larger than the 2048-byte RX buffer, causing the GDMA BNIC emulator
    // to return CQE_RX_TRUNCATED, exercising the rx_poll error path.
    let expected_num_tx_packets = 1;
    let expected_num_rx_packets = 0;
    let num_segments = 1;
    let packet_len = 4096; // Exceeds the 2048-byte RX buffer

    let mut pkt_builder = TxPacketBuilder::new();
    build_tx_segments(packet_len, num_segments, false, &mut pkt_builder);

    let (stats, _) = test_endpoint(
        driver,
        GuestDmaMode::DirectDma,
        &pkt_builder,
        expected_num_tx_packets,
        expected_num_rx_packets,
        ManaTestConfiguration::default(),
    )
    .await;

    assert_eq!(stats.rx_errors.get(), 1, "rx_errors should increase");
    assert_eq!(stats.rx_packets.get(), 0, "rx_packets should stay the same");
    assert_eq!(stats.tx_packets.get(), 1, "tx_packets should increase");
}

>>>>>>> f9521b06 (netvsp: Add VLAN support (#3417))
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

    let (stats, _) = test_endpoint(
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

fn build_tx_segments_internal(
    packet_len: usize,
    num_segments: usize,
    enable_lso: bool,
<<<<<<< HEAD
) -> (Vec<u8>, Vec<TxSegment>) {
    // Packet length must be divisible by number of segments.
=======
    vlan: Option<VlanMetadata>,
    pkt_builder: &mut TxPacketBuilder,
) {
>>>>>>> f9521b06 (netvsp: Add VLAN support (#3417))
    assert_eq!(packet_len % num_segments, 0);
    let data_to_send = (0..packet_len).map(|v| v as u8).collect::<Vec<u8>>();
    let tx_id = 1;
    let mut tx_segments = Vec::new();
    let segment_len = packet_len / num_segments;
    let mut tx_metadata = net_backend::TxMetadata {
        id: TxId(tx_id),
        segment_count: num_segments as u8,
        len: packet_len as u32,
<<<<<<< HEAD
        l2_len: 14,                 // Ethernet header
        l3_len: 20,                 // IPv4 header
        l4_len: 20,                 // TCP header
        max_tcp_segment_size: 1460, // Typical MSS for Ethernet
=======
        l2_len: if vlan.is_some() {
            18 // Ethernet header with 802.1q
        } else {
            14 // Ethernet header
        },
        l3_len: 20,             // IPv4 header
        l4_len: 20,             // TCP header
        max_segment_size: 1460, // Typical MSS for Ethernet
        vlan,
>>>>>>> f9521b06 (netvsp: Add VLAN support (#3417))
        ..Default::default()
    };

    tx_metadata.flags.set_offload_tcp_segmentation(enable_lso);

    if tx_metadata.vlan.is_some() {
        assert_eq!(
            tx_metadata.l2_len as usize + tx_metadata.l3_len as usize + tx_metadata.l4_len as usize,
            IPV4_VLAN_HEADER_LENGTH
        );
    } else {
        assert_eq!(
            tx_metadata.l2_len as usize + tx_metadata.l3_len as usize + tx_metadata.l4_len as usize,
            IPV4_HEADER_LENGTH
        );
    }

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

fn build_tx_segments(
    packet_len: usize,
    num_segments: usize,
    enable_lso: bool,
    pkt_builder: &mut TxPacketBuilder,
) {
    build_tx_segments_internal(packet_len, num_segments, enable_lso, None, pkt_builder);
}

fn build_tx_segments_vlan(
    packet_len: usize,
    num_segments: usize,
    vlan_id: u16,
    vlan_priority: u8,
    vlan_dei: bool,
    pkt_builder: &mut TxPacketBuilder,
) {
    build_tx_segments_internal(
        packet_len,
        num_segments,
        false, // LSO doesn't make sense for VLAN-tagged packets in these tests.
        Some(
            VlanMetadata::new()
                .with_priority(vlan_priority)
                .with_drop_eligible_indicator(vlan_dei)
                .with_vlan_id(vlan_id),
        ),
        pkt_builder,
    );
}

async fn test_endpoint(
    driver: DefaultDriver,
    dma_mode: GuestDmaMode,
    packet_len: usize,
    tx_segments: Vec<TxSegment>,
    data_to_send: Vec<u8>,
    expected_num_received_packets: usize,
    test_configuration: ManaTestConfiguration,
<<<<<<< HEAD
) -> QueueStats {
    let tx_id = 1;
=======
) -> (QueueStats, Vec<Option<net_backend::RxMetadata>>) {
>>>>>>> f9521b06 (netvsp: Add VLAN support (#3417))
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
        return (stats, Vec::new());
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

    // Gather per-buffer RX metadata written by net_mana.
    let rx_meta: Vec<Option<net_backend::RxMetadata>> = rx_packets[..rx_packets_n]
        .iter()
        .map(|id| pool.rx_metadata(*id))
        .collect();

    let stats = get_queue_stats(queues[0].queue_stats());
    drop(queues);
    endpoint.stop().await;
    (stats, rx_meta)
}

fn get_queue_stats(queue_stats: Option<&dyn net_backend::BackendQueueStats>) -> QueueStats {
    let queue_stats = queue_stats.unwrap();
    QueueStats {
        rx_errors: queue_stats.rx_errors(),
        tx_errors: queue_stats.tx_errors(),
        rx_packets: queue_stats.rx_packets(),
        tx_packets: queue_stats.tx_packets(),
        tx_vlan_packets: queue_stats.tx_vlan_packets(),
        rx_vlan_packets: queue_stats.rx_vlan_packets(),
        ..Default::default()
    }
}
<<<<<<< HEAD
=======

use crate::ManaQueue;
use gdma_defs::CqeParams;
use gdma_defs::bnic::ManaTxCompOob;
use mana_driver::mana::ResourceArena;
use page_pool_alloc::PagePoolAllocator;
use zerocopy::FromZeros;

type TestEmulatedDevice = EmulatedDevice<gdma::GdmaDevice, PagePoolAllocator>;

/// Sets up the full device stack and returns a [`ManaQueue`] ready for
/// direct `handle_tx_cqe` testing along with the resources needed for
/// teardown.
async fn new_test_queue(
    driver: &DefaultDriver,
) -> (
    ManaQueue<TestEmulatedDevice>,
    ResourceArena,
    ManaEndpoint<TestEmulatedDevice>,
) {
    let pages = 256;
    let mem = DeviceTestMemory::new(pages * 2, true, "test queue");
    let msi_conn = MsiConnection::new(AssignedBusRange::new(), 0);
    let device = gdma::GdmaDevice::new(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        mem.guest_memory(),
        msi_conn.target(),
        vec![VportConfig {
            mac_address: [1, 2, 3, 4, 5, 6].into(),
            endpoint: Box::new(LoopbackEndpoint::new()),
        }],
        &mut ExternallyManagedMmioIntercepts,
    );
    let device = EmulatedDevice::new(device, msi_conn, mem.dma_client());
    let dev_config = ManaQueryDeviceCfgResp {
        pf_cap_flags1: 0.into(),
        pf_cap_flags2: 0,
        pf_cap_flags3: 0,
        pf_cap_flags4: 0,
        max_num_vports: 1,
        reserved: 0,
        max_num_eqs: 64,
        adapter_mtu: 0,
        reserved2: 0,
        adapter_link_speed_mbps: 0,
    };
    let thing = ManaDevice::new(driver, device, 1, 1, None).await.unwrap();
    let vport = thing.new_vport(0, None, &dev_config).await.unwrap();
    let mut endpoint = ManaEndpoint::new(driver.clone(), vport, GuestDmaMode::DirectDma).await;
    let tx_config = endpoint.vport.config_tx().await.unwrap();

    let mut arena = ResourceArena::new();
    let (queue, _resources) = endpoint.new_queue(&tx_config, &mut arena, 0).await.unwrap();

    (queue, arena, endpoint)
}

#[async_test]
#[should_panic(expected = "TX CQE arrived with no matching posted TX")]
async fn tx_spurious_cqe_panics(driver: DefaultDriver) {
    use gdma_defs::bnic::CQE_TX_OKAY;

    let (mut queue, _arena, _endpoint) = new_test_queue(&driver).await;

    assert!(queue.posted_tx.is_empty());
    let mut oob = ManaTxCompOob::new_zeroed();
    oob.cqe_hdr.set_cqe_type(CQE_TX_OKAY);

    let _ = queue.handle_tx_cqe(&oob, CqeParams::new(), 8);
}

#[async_test]
async fn tx_cqe_gdma_err_returns_try_restart(driver: DefaultDriver) {
    use crate::PostedTx;
    use gdma_defs::bnic::CQE_TX_GDMA_ERR;
    use net_backend::TxError;

    let (mut queue, arena, mut endpoint) = new_test_queue(&driver).await;

    queue.posted_tx.push_back(PostedTx {
        id: TxId(42),
        wqe_len: 0,
        bounced_len_with_padding: 0,
    });

    let mut oob = ManaTxCompOob::new_zeroed();
    oob.cqe_hdr.set_cqe_type(CQE_TX_GDMA_ERR);

    // CQE_TX_GDMA_ERR returns TryRestart without popping posted_tx.
    let result = queue.handle_tx_cqe(&oob, CqeParams::new(), 8);
    assert!(
        matches!(result, Err(TxError::TryRestart(_))),
        "expected TryRestart, got {result:?}"
    );
    assert_eq!(queue.stats.tx_errors.get(), 1);
    assert_eq!(queue.stats.tx_stuck.get(), 1);
    assert_eq!(queue.posted_tx.len(), 1);

    drop(queue);
    endpoint.vport.destroy(arena).await;
    endpoint.stop().await;
}

#[async_test]
async fn tx_cqe_invalid_oob_completes_packet(driver: DefaultDriver) {
    use crate::PostedTx;
    use gdma_defs::bnic::CQE_TX_INVALID_OOB;

    let (mut queue, arena, mut endpoint) = new_test_queue(&driver).await;

    queue.posted_tx.push_back(PostedTx {
        id: TxId(7),
        wqe_len: 0,
        bounced_len_with_padding: 0,
    });

    let mut oob = ManaTxCompOob::new_zeroed();
    oob.cqe_hdr.set_cqe_type(CQE_TX_INVALID_OOB);

    // CQE_TX_INVALID_OOB logs an error but still pops posted_tx.
    let result = queue.handle_tx_cqe(&oob, CqeParams::new(), 8);
    assert_eq!(result.unwrap().0, 7);
    assert_eq!(queue.stats.tx_errors.get(), 1);
    assert!(queue.posted_tx.is_empty());

    drop(queue);
    endpoint.vport.destroy(arena).await;
    endpoint.stop().await;
}

#[async_test]
async fn tx_cqe_okay_completes_packet(driver: DefaultDriver) {
    use crate::PostedTx;
    use gdma_defs::bnic::CQE_TX_OKAY;

    let (mut queue, arena, mut endpoint) = new_test_queue(&driver).await;

    queue.posted_tx.push_back(PostedTx {
        id: TxId(99),
        wqe_len: 0,
        bounced_len_with_padding: 0,
    });

    let mut oob = ManaTxCompOob::new_zeroed();
    oob.cqe_hdr.set_cqe_type(CQE_TX_OKAY);

    let result = queue.handle_tx_cqe(&oob, CqeParams::new(), 8);
    assert_eq!(result.unwrap().0, 99);
    assert_eq!(queue.stats.tx_packets.get(), 1);
    assert!(queue.posted_tx.is_empty());

    drop(queue);
    endpoint.vport.destroy(arena).await;
    endpoint.stop().await;
}

// ---------------------------------------------------------------------------
// VLAN tests
// ---------------------------------------------------------------------------

/// Verify that a single VLAN-tagged packet round-trips through the MANA TX and
/// RX paths with the VLAN ID preserved.
#[async_test]
async fn test_vlan_tx_rx_roundtrip_direct_dma(driver: DefaultDriver) {
    let mut pkt_builder = TxPacketBuilder::new();
    build_tx_segments_vlan(1138, 1, 42, 0, false, &mut pkt_builder);

    let (stats, rx_meta) = test_endpoint(
        driver,
        GuestDmaMode::DirectDma,
        &pkt_builder,
        1, // expected TX
        1, // expected RX
        ManaTestConfiguration::default(),
    )
    .await;

    assert_eq!(stats.tx_packets.get(), 1);
    assert_eq!(stats.rx_packets.get(), 1);
    assert_eq!(stats.tx_vlan_packets.get(), 1);
    assert_eq!(stats.rx_vlan_packets.get(), 1);

    let rx_vlan = rx_meta[0]
        .expect("RX metadata should be present")
        .vlan
        .expect("RX metadata should carry VLAN");
    assert_eq!(rx_vlan.vlan_id(), 42);
    assert_eq!(rx_vlan.priority(), 0);
    assert_eq!(rx_vlan.drop_eligible_indicator(), false);
}

/// Same round-trip but with bounce-buffer DMA mode.
#[async_test]
async fn test_vlan_tx_rx_roundtrip_bounce_buffer(driver: DefaultDriver) {
    let mut pkt_builder = TxPacketBuilder::new();
    build_tx_segments_vlan(1138, 1, 99, 0, false, &mut pkt_builder);

    let (stats, rx_meta) = test_endpoint(
        driver,
        GuestDmaMode::BounceBuffer,
        &pkt_builder,
        1,
        1,
        ManaTestConfiguration::default(),
    )
    .await;

    assert_eq!(stats.tx_packets.get(), 1);
    assert_eq!(stats.rx_packets.get(), 1);
    assert_eq!(stats.tx_vlan_packets.get(), 1);
    assert_eq!(stats.rx_vlan_packets.get(), 1);

    let rx_vlan = rx_meta[0]
        .expect("RX metadata should be present")
        .vlan
        .expect("RX metadata should carry VLAN");
    assert_eq!(rx_vlan.vlan_id(), 99);
    assert_eq!(rx_vlan.priority(), 0);
    assert_eq!(rx_vlan.drop_eligible_indicator(), false);
}

/// Verify that a non-VLAN packet does NOT produce VLAN metadata.
#[async_test]
async fn test_no_vlan_rx_metadata_when_untagged(driver: DefaultDriver) {
    let mut pkt_builder = TxPacketBuilder::new();
    build_tx_segments(1138, 1, false, &mut pkt_builder);

    let (stats, rx_meta) = test_endpoint(
        driver,
        GuestDmaMode::DirectDma,
        &pkt_builder,
        1,
        1,
        ManaTestConfiguration::default(),
    )
    .await;

    assert_eq!(stats.tx_vlan_packets.get(), 0);
    assert_eq!(stats.rx_vlan_packets.get(), 0);

    let rx = rx_meta[0].expect("RX metadata should be present");
    assert!(
        rx.vlan.is_none(),
        "RX metadata must not carry VLAN for an untagged packet"
    );
}

/// Mix of VLAN-tagged and untagged packets in a single TX batch.
#[async_test]
async fn test_vlan_mixed_batch(driver: DefaultDriver) {
    let mut pkt_builder = TxPacketBuilder::new();

    // Packet 0: no VLAN
    build_tx_segments(550, 1, false, &mut pkt_builder);
    // Packet 1: VLAN 100
    build_tx_segments_vlan(550, 1, 100, 0, false, &mut pkt_builder);
    // Packet 2: no VLAN, multi-segment
    build_tx_segments(1130, 10, false, &mut pkt_builder);
    // Packet 3: VLAN 4094 (max 12-bit value)
    build_tx_segments_vlan(550, 1, 4094, 0, false, &mut pkt_builder);

    let (stats, rx_meta) = test_endpoint(
        driver,
        GuestDmaMode::DirectDma,
        &pkt_builder,
        4,
        4,
        ManaTestConfiguration::default(),
    )
    .await;

    assert_eq!(stats.tx_packets.get(), 4);
    assert_eq!(stats.rx_packets.get(), 4);
    assert_eq!(stats.tx_vlan_packets.get(), 2);
    assert_eq!(stats.rx_vlan_packets.get(), 2);

    // Packet 0: no VLAN
    assert!(
        rx_meta[0]
            .expect("RX metadata should be present")
            .vlan
            .is_none()
    );

    // Packet 1: VLAN 100
    assert_eq!(
        rx_meta[1]
            .expect("RX metadata should be present")
            .vlan
            .expect("RX should carry VLAN")
            .vlan_id(),
        100
    );
    assert_eq!(
        rx_meta[1]
            .expect("RX metadata should be present")
            .vlan
            .expect("RX should carry VLAN")
            .priority(),
        0
    );
    assert_eq!(
        rx_meta[1]
            .expect("RX metadata should be present")
            .vlan
            .expect("RX should carry VLAN")
            .drop_eligible_indicator(),
        false
    );

    // Packet 2: no VLAN
    assert!(
        rx_meta[2]
            .expect("RX metadata should be present")
            .vlan
            .is_none()
    );

    // Packet 3: VLAN 4094
    assert_eq!(
        rx_meta[3]
            .expect("RX metadata should be present")
            .vlan
            .expect("RX should carry VLAN")
            .vlan_id(),
        4094
    );
    assert_eq!(
        rx_meta[3]
            .expect("RX metadata should be present")
            .vlan
            .expect("RX should carry VLAN")
            .priority(),
        0
    );
    assert_eq!(
        rx_meta[3]
            .expect("RX metadata should be present")
            .vlan
            .expect("RX should carry VLAN")
            .drop_eligible_indicator(),
        false
    );
}
>>>>>>> f9521b06 (netvsp: Add VLAN support (#3417))
