// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Saved-state tests: stopping the queues rewinds unused RX descriptors,
//! partial queue lifecycles are saved and validated, and an inactive transport
//! defers the device-private state until the device is activated.

use super::QUEUE_SIZE;
use super::RX_AVAIL_ADDR;
use super::RX_DESC_ADDR;
use super::RX_USED_ADDR;
use super::TX_AVAIL_ADDR;
use super::TX_DESC_ADDR;
use super::TX_USED_ADDR;
use super::TestHarness;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use pal_async::DefaultDriver;
use pal_async::async_test;
use test_with_tracing::test;
use virtio::VirtioDevice;
use virtio::device::saved_state::DeviceQueueState;
use virtio::queue::QueueParams;
use virtio::spec::VirtioDeviceFeatures;
use virtio::transport::VirtioMmioDevice;
use vmcore::device_state::ChangeDeviceState;
use vmcore::line_interrupt::LineInterrupt;
use vmcore::save_restore::SaveRestore;

#[async_test]
async fn save_restore_rewinds_unused_rx_and_validates_private_state(driver: DefaultDriver) {
    let mut harness = TestHarness::new_save_restore(&driver);
    let mut handle = harness.enable_and_get_handle().await;
    harness.post_rx_buffer_and_signal(0, 1500);
    handle.wait_for_rx_pending().await;

    harness.device.quiesce_input().await.unwrap();
    let rx_state = harness.device.stop_queue(0).await.unwrap();
    let tx_state = harness.device.stop_queue(1).await.unwrap();
    assert_eq!(rx_state.avail_index, rx_state.used_index);
    assert_eq!(tx_state.avail_index, tx_state.used_index);
    let saved = harness.device.save_device().unwrap().unwrap();

    let queues = [
        DeviceQueueState {
            params: QueueParams {
                size: QUEUE_SIZE,
                enable: true,
                desc_addr: RX_DESC_ADDR,
                avail_addr: RX_AVAIL_ADDR,
                used_addr: RX_USED_ADDR,
            },
            queue_state: Some(rx_state),
        },
        DeviceQueueState {
            params: QueueParams {
                size: QUEUE_SIZE,
                enable: true,
                desc_addr: TX_DESC_ADDR,
                avail_addr: TX_AVAIL_ADDR,
                used_addr: TX_USED_ADDR,
            },
            queue_state: Some(tx_state),
        },
    ];
    let restored = TestHarness::new_save_restore(&driver);
    restored.device.device_state_validator()(
        Some(&saved),
        &VirtioDeviceFeatures::new(),
        &queues,
        &harness.mem,
    )
    .unwrap();
    let mut restored_device = restored.device;
    restored_device.restore_device(Some(saved)).unwrap();
    assert_eq!(restored_device.lifecycle.endpoint_generation, 1);
}

#[async_test]
async fn save_restore_preserves_partial_queue_lifecycle(driver: DefaultDriver) {
    let features = VirtioDeviceFeatures::new().with_bank(0, 1 << 5);
    let disabled_queues = [
        DeviceQueueState {
            params: QueueParams {
                size: QUEUE_SIZE,
                enable: false,
                desc_addr: RX_DESC_ADDR,
                avail_addr: RX_AVAIL_ADDR,
                used_addr: RX_USED_ADDR,
            },
            queue_state: None,
        },
        DeviceQueueState {
            params: QueueParams {
                size: QUEUE_SIZE,
                enable: false,
                desc_addr: TX_DESC_ADDR,
                avail_addr: TX_AVAIL_ADDR,
                used_addr: TX_USED_ADDR,
            },
            queue_state: None,
        },
    ];

    let mut unstarted = TestHarness::new_save_restore(&driver);
    let unstarted_state = unstarted.device.save_device().unwrap().unwrap();
    unstarted.device.device_state_validator()(
        Some(&unstarted_state),
        &features,
        &disabled_queues,
        &unstarted.mem,
    )
    .unwrap();
    let mut configured_queues = disabled_queues;
    configured_queues[0].params.enable = true;
    configured_queues[1].params.enable = true;
    unstarted.device.device_state_validator()(
        Some(&unstarted_state),
        &features,
        &configured_queues,
        &unstarted.mem,
    )
    .unwrap();

    let mut half_open = TestHarness::new_save_restore(&driver);
    half_open.start_rx_queue(&features).await;
    let rx_state = half_open.device.stop_queue(0).await.unwrap();
    let half_open_state = half_open.device.save_device().unwrap().unwrap();
    let mut partial_queues = disabled_queues;
    partial_queues[0].params.enable = true;
    partial_queues[0].queue_state = Some(rx_state);
    half_open.device.device_state_validator()(
        Some(&half_open_state),
        &features,
        &partial_queues,
        &half_open.mem,
    )
    .unwrap();
}

#[async_test]
async fn inactive_transport_restore_defers_net_private_state(driver: DefaultDriver) {
    let source_harness = TestHarness::new_save_restore(&driver);
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
    let saved = source.save().unwrap();
    assert_eq!(
        saved
            .device_state
            .as_ref()
            .unwrap()
            .parse::<crate::saved_state::SavedState>()
            .unwrap()
            .endpoint_generation,
        0
    );

    let destination_harness = TestHarness::new_save_restore(&driver);
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
    assert_eq!(
        staged
            .device_state
            .as_ref()
            .unwrap()
            .parse::<crate::saved_state::SavedState>()
            .unwrap()
            .endpoint_generation,
        0
    );

    destination.start_fallible().await.unwrap();
    let mut config = [0; 4];
    match destination.mmio_read(0x100, &mut config) {
        IoResult::Defer(token) => token.read_future(&mut config).await.unwrap(),
        other => panic!("expected deferred config read, got {other:?}"),
    }
    destination.stop().await;
    let activated = destination.save().unwrap();
    assert_eq!(
        activated
            .device_state
            .unwrap()
            .parse::<crate::saved_state::SavedState>()
            .unwrap()
            .endpoint_generation,
        1
    );
}
