// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for saving, validating, and restoring the device-private console state.

use super::DATA_BASE;
use super::QUEUE_SIZE;
use super::TOTAL_MEM_SIZE;
use super::TX_AVAIL_ADDR;
use super::TX_DESC_ADDR;
use super::TX_USED_ADDR;
use super::TestHarness;
use super::harness::BROKER_CAPABILITY;
use super::harness::BROKER_INSTANCE;
use super::harness::encode;
use super::harness::yield_until;
use super::new_mock_serial;
use crate::VirtioConsoleDevice;
use crate::control_session_protocol::Record;
use crate::control_session_protocol::RecordType;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use guestmem::GuestMemory;
use pal_async::DefaultDriver;
use pal_async::async_test;
use test_with_tracing::test;
use virtio::VirtioDevice;
use virtio::device::saved_state::DeviceQueueState;
use virtio::queue::QueueParams;
use virtio::queue::QueueState;
use virtio::spec::VirtioDeviceFeatures;
use virtio::spec::queue::DescriptorFlags;
use virtio::test_helpers::init_avail_ring;
use virtio::test_helpers::init_used_ring;
use virtio::test_helpers::make_available;
use virtio::test_helpers::write_descriptor;
use virtio::transport::VirtioMmioDevice;
use virtio_resources::console::attachment::VirtioConsoleDisconnectPolicy;
use vmcore::device_state::ChangeDeviceState;
use vmcore::line_interrupt::LineInterrupt;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateBlob;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;

#[async_test]
async fn staged_rx_restores_before_new_endpoint_bytes(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;

    harness.handle.inject_rx_data(b"before-snapshot");
    yield_until(|| harness.handle.pending_rx_len() == 0).await;

    let receive_state = harness.device.stop_queue(0).await.unwrap();
    let transmit_state = harness.device.stop_queue(1).await.unwrap();
    let saved = harness.device.save_device().unwrap().unwrap();

    harness.replace_device(VirtioConsoleDisconnectPolicy::Discard);
    harness.device.restore_device(Some(saved)).unwrap();
    harness
        .enable_with_state(Some(receive_state), Some(transmit_state))
        .await;
    harness.handle.inject_rx_data(b"after-restore");

    let first_gpa = harness.post_rx_buffer_and_signal(0, 64);
    let (_, first_len) = harness.wait_for_rx_used().await;
    let mut first = vec![0; first_len as usize];
    harness.mem.read_at(first_gpa, &mut first).unwrap();
    assert_eq!(first, b"before-snapshot");

    let second_gpa = harness.post_rx_buffer_and_signal(1, 64);
    let (_, second_len) = harness.wait_for_rx_used().await;
    let mut second = vec![0; second_len as usize];
    harness.mem.read_at(second_gpa, &mut second).unwrap();
    assert_eq!(second, b"after-restore");
}

#[async_test]
async fn partial_tx_restores_without_replay(driver: DefaultDriver) {
    let mut harness = TestHarness::new_with_policy(&driver, VirtioConsoleDisconnectPolicy::Retain);
    harness.enable().await;
    harness.handle.set_write_limit_then_disconnect(3);
    harness.post_tx_and_signal(0, b"abcdef");
    yield_until(|| harness.handle.tx_data() == b"abc").await;

    let receive_state = harness.device.stop_queue(0).await.unwrap();
    let transmit_state = harness.device.stop_queue(1).await.unwrap();
    let saved = harness.device.save_device().unwrap().unwrap();

    harness.replace_device(VirtioConsoleDisconnectPolicy::Retain);
    harness.device.restore_device(Some(saved)).unwrap();
    harness
        .enable_with_state(Some(receive_state), Some(transmit_state))
        .await;

    let (used_id, _) = harness.wait_for_tx_used().await;
    assert_eq!(used_id, 0);
    assert_eq!(harness.handle.take_tx_data(), b"def");
}

#[async_test]
async fn saved_state_validator_rejects_wrong_schema(driver: DefaultDriver) {
    let (io, _) = new_mock_serial();
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let device = VirtioConsoleDevice::new(&driver_source, Box::new(io));
    let validator = device.device_state_validator();
    let state = SavedStateBlob::new(crate::saved_state::SavedState {
        schema_version: crate::saved_state::SAVED_STATE_VERSION + 1,
        columns: 0,
        rows: 0,
        partial_transmit: 0,
        staged_rx: Vec::new(),
        disconnect_policy_id: 0,
        broker: None,
    });
    assert!(
        validator(
            Some(&state),
            &VirtioDeviceFeatures::new(),
            &[],
            &GuestMemory::empty(),
        )
        .is_err()
    );
}

#[async_test]
async fn saved_state_validator_rejects_tx_offset_past_descriptor(driver: DefaultDriver) {
    let mem = GuestMemory::allocate(TOTAL_MEM_SIZE);
    init_avail_ring(&mem, TX_AVAIL_ADDR);
    init_used_ring(&mem, TX_USED_ADDR);
    let payload_gpa = DATA_BASE;
    mem.write_at(payload_gpa, b"short").unwrap();
    write_descriptor(
        &mem,
        TX_DESC_ADDR,
        0,
        payload_gpa,
        5,
        DescriptorFlags::new(),
        0,
    );
    let mut avail_index = 0;
    make_available(&mem, TX_AVAIL_ADDR, QUEUE_SIZE, 0, &mut avail_index);

    let (io, _) = new_mock_serial();
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let device = VirtioConsoleDevice::new(&driver_source, Box::new(io));
    let validator = device.device_state_validator();
    let state = SavedStateBlob::new(crate::saved_state::SavedState {
        schema_version: crate::saved_state::SAVED_STATE_VERSION,
        columns: 0,
        rows: 0,
        partial_transmit: 6,
        staged_rx: Vec::new(),
        disconnect_policy_id: 0,
        broker: None,
    });
    let queues = [
        DeviceQueueState {
            params: QueueParams::default(),
            queue_state: None,
        },
        DeviceQueueState {
            params: QueueParams {
                size: QUEUE_SIZE,
                enable: true,
                desc_addr: TX_DESC_ADDR,
                avail_addr: TX_AVAIL_ADDR,
                used_addr: TX_USED_ADDR,
            },
            queue_state: Some(QueueState::default()),
        },
    ];
    assert!(validator(Some(&state), &VirtioDeviceFeatures::new(), &queues, &mem,).is_err());
}

#[async_test]
async fn inactive_transport_restore_defers_console_private_state(driver: DefaultDriver) {
    let source_harness = TestHarness::new(&driver);
    let mut source = VirtioMmioDevice::new(
        Box::new(source_harness.device),
        &driver,
        source_harness.mem,
        LineInterrupt::detached(),
        None,
        0,
        0x1000,
    )
    .unwrap();
    source.stop().await;
    let mut saved = source.save().unwrap();
    let staged_rx = b"deferred console input".to_vec();
    saved.device_state = Some(SavedStateBlob::new(crate::saved_state::SavedState {
        schema_version: crate::saved_state::SAVED_STATE_VERSION,
        columns: 123,
        rows: 45,
        partial_transmit: 0,
        staged_rx: staged_rx.clone(),
        disconnect_policy_id: 0,
        broker: None,
    }));

    let destination_harness = TestHarness::new(&driver);
    let mut destination = VirtioMmioDevice::new(
        Box::new(destination_harness.device),
        &driver,
        destination_harness.mem,
        LineInterrupt::detached(),
        None,
        0,
        0x1000,
    )
    .unwrap();
    destination.restore(saved).unwrap();
    destination.start_fallible().await.unwrap();
    destination.stop().await;
    let staged = destination.save().unwrap();
    let staged_private = staged
        .device_state
        .as_ref()
        .unwrap()
        .parse::<crate::saved_state::SavedState>()
        .unwrap();
    assert_eq!(staged_private.columns, 123);
    assert_eq!(staged_private.rows, 45);
    assert_eq!(staged_private.staged_rx, staged_rx);

    destination.start_fallible().await.unwrap();
    let mut config = [0; 4];
    match destination.mmio_read(0x100, &mut config) {
        IoResult::Defer(token) => token.read_future(&mut config).await.unwrap(),
        other => panic!("expected deferred config read, got {other:?}"),
    }
    assert_eq!(u16::from_ne_bytes(config[..2].try_into().unwrap()), 123);
    assert_eq!(u16::from_ne_bytes(config[2..].try_into().unwrap()), 45);

    let mut invalid = staged;
    invalid.device_state = Some(SavedStateBlob::new(crate::saved_state::SavedState {
        schema_version: crate::saved_state::SAVED_STATE_VERSION,
        columns: 80,
        rows: 25,
        partial_transmit: 0,
        staged_rx: vec![0; crate::saved_state::MAX_STAGED_RX_BYTES + 1],
        disconnect_policy_id: 0,
        broker: None,
    }));
    let invalid_harness = TestHarness::new(&driver);
    let mut invalid_destination = VirtioMmioDevice::new(
        Box::new(invalid_harness.device),
        &driver,
        invalid_harness.mem,
        LineInterrupt::detached(),
        None,
        0,
        0x1000,
    )
    .unwrap();
    assert!(invalid_destination.restore(invalid).is_err());
}

#[async_test]
async fn direct_and_broker_saved_state_schemas_are_not_interchangeable(driver: DefaultDriver) {
    let mut direct = TestHarness::new(&driver);
    direct.enable().await;
    direct.device.stop_queue(0).await;
    direct.device.stop_queue(1).await;
    let direct_saved = direct.device.save_device().unwrap().unwrap();
    let direct_state: crate::saved_state::SavedState = direct_saved.parse().unwrap();
    assert_eq!(
        direct_state.schema_version,
        crate::saved_state::DIRECT_SAVED_STATE_VERSION
    );
    assert!(direct_state.broker.is_none());

    let mut broker = TestHarness::new_broker(&driver, BROKER_INSTANCE, BROKER_CAPABILITY);
    assert!(broker.device.restore_device(Some(direct_saved)).is_err());

    broker.enable().await;
    broker.device.stop_queue(0).await;
    broker.device.stop_queue(1).await;
    let broker_saved = broker.device.save_device().unwrap().unwrap();
    let broker_state: crate::saved_state::SavedState = broker_saved.parse().unwrap();
    assert_eq!(
        broker_state.schema_version,
        crate::saved_state::BROKER_SAVED_STATE_VERSION
    );
    assert!(broker_state.broker.is_some());

    let (io, _) = new_mock_serial();
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let mut direct_device = VirtioConsoleDevice::new(&driver_source, Box::new(io));
    assert!(direct_device.restore_device(Some(broker_saved)).is_err());
}

#[async_test]
async fn direct_schema_v1_allows_staged_rx_without_receive_queue(driver: DefaultDriver) {
    let (io, _) = new_mock_serial();
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let device = VirtioConsoleDevice::new(&driver_source, Box::new(io));
    let state = crate::saved_state::SavedState {
        schema_version: crate::saved_state::DIRECT_SAVED_STATE_VERSION,
        columns: 0,
        rows: 0,
        partial_transmit: 0,
        staged_rx: vec![1],
        disconnect_policy_id: 0,
        broker: None,
    };
    let saved = SavedStateBlob::new(state);
    let validator = device.device_state_validator();
    assert!(
        validator(
            Some(&saved),
            &VirtioDeviceFeatures::new(),
            &[],
            &GuestMemory::empty(),
        )
        .is_ok()
    );
}

#[async_test]
async fn oversized_saved_state_is_rejected_before_nested_decode(driver: DefaultDriver) {
    let (io, _) = new_mock_serial();
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let device = VirtioConsoleDevice::new(&driver_source, Box::new(io));
    let state = crate::saved_state::SavedState {
        schema_version: crate::saved_state::DIRECT_SAVED_STATE_VERSION,
        columns: 0,
        rows: 0,
        partial_transmit: 0,
        staged_rx: vec![0; crate::saved_state::MAX_SAVED_STATE_BYTES],
        disconnect_policy_id: 0,
        broker: None,
    };
    let saved = SavedStateBlob::new(state);
    assert!(saved.encoded_len() > crate::saved_state::MAX_SAVED_STATE_BYTES);
    let validator = device.device_state_validator();
    assert!(
        validator(
            Some(&saved),
            &VirtioDeviceFeatures::new(),
            &[],
            &GuestMemory::empty(),
        )
        .is_err()
    );
}

#[async_test]
async fn broker_state_validator_rejects_malformed_private_state(driver: DefaultDriver) {
    let mut harness = TestHarness::new_broker(&driver, BROKER_INSTANCE, BROKER_CAPABILITY);
    harness.enable().await;
    harness.device.stop_queue(0).await;
    harness.device.stop_queue(1).await;
    let saved = harness.device.save_device().unwrap().unwrap();
    let mut state: crate::saved_state::SavedState = saved.parse().unwrap();
    state.broker.as_mut().unwrap().guest_parser.header_count = u32::MAX;
    let malformed = SavedStateBlob::new(state);
    let validator = harness.device.device_state_validator();
    assert!(
        validator(
            Some(&malformed),
            &VirtioDeviceFeatures::new(),
            &[],
            &GuestMemory::empty(),
        )
        .is_err()
    );

    let mut state: crate::saved_state::SavedState = saved.parse().unwrap();
    let broker = state.broker.as_mut().unwrap();
    broker.instance_id = [0x44; 16].to_vec();
    broker.guest_output.current = Some(crate::saved_state::SavedEncodedRecord {
        bytes: encode(&Record::session(
            RecordType::Reset,
            BROKER_INSTANCE,
            1,
            0,
            Vec::new(),
        )),
        offset: 1,
    });
    let malformed = SavedStateBlob::new(state);
    assert!(
        validator(
            Some(&malformed),
            &VirtioDeviceFeatures::new(),
            &[],
            &GuestMemory::empty(),
        )
        .is_err()
    );
}
