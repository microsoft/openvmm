// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Input quiesce tests: a quiesce requested before the queues start is latched,
//! and quiesce drains the TX work that the endpoint already accepted.

use super::TestHarness;
use net_backend::TxId;
use pal_async::DefaultDriver;
use pal_async::async_test;
use test_with_tracing::test;
use virtio::VirtioDevice;

#[async_test]
async fn input_quiesce_latches_before_queue_start(driver: DefaultDriver) {
    let mut harness = TestHarness::new_save_restore(&driver);
    harness.device.quiesce_input().await.unwrap();

    let mut handle = harness.enable_and_get_handle().await;
    handle.wait_for_quiesce().await;

    harness.device.resume_input().await.unwrap();
}

#[async_test]
async fn snapshot_drains_accepted_pending_tx_exactly_once(driver: DefaultDriver) {
    let mut harness = TestHarness::new_save_restore(&driver);
    let mut handle = harness.enable_and_get_handle().await;
    handle.tx_avail_behavior.lock().sync = false;

    harness.post_tx_and_signal(0, 64);
    handle.wait_for_tx_avail().await;
    handle.tx_completions.lock().push_back(vec![TxId(0)]);

    harness.device.quiesce_input().await.unwrap();
    let rx_state = harness.device.stop_queue(0).await.unwrap();
    let tx_state = harness.device.stop_queue(1).await.unwrap();
    assert_eq!(rx_state.avail_index, rx_state.used_index);
    assert_eq!(tx_state.avail_index, tx_state.used_index);
    harness.device.save_device().unwrap().unwrap();
    assert!(handle.tx_completions.lock().is_empty());
}
