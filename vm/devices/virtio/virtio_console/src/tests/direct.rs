// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for input gating and read-error handling in the direct worker mode.

use super::TestHarness;
use super::harness::yield_now;
use super::harness::yield_until;
use pal_async::DefaultDriver;
use pal_async::async_test;
use test_with_tracing::test;
use virtio::VirtioDevice;
use virtio_resources::console::attachment::VirtioConsoleDisconnectPolicy;

#[async_test]
async fn input_gate_cancels_rx_but_keeps_tx_running(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;
    harness.device.quiesce_input().await.unwrap();

    harness.handle.inject_rx_data(b"gated-input");
    for _ in 0..10 {
        yield_now().await;
    }
    assert_eq!(harness.handle.pending_rx_len(), b"gated-input".len());

    harness.post_tx_and_signal(0, b"tx-while-gated");
    let (used_id, _) = harness.wait_for_tx_used().await;
    assert_eq!(used_id, 0);
    assert_eq!(harness.handle.take_tx_data(), b"tx-while-gated");

    harness.device.resume_input().await.unwrap();
    let gpa = harness.post_rx_buffer_and_signal(0, 64);
    let (_, used_len) = harness.wait_for_rx_used().await;
    let mut received = vec![0; used_len as usize];
    harness.mem.read_at(gpa, &mut received).unwrap();
    assert_eq!(received, b"gated-input");
}

#[async_test]
async fn read_error_drives_disconnect_before_reconnect(driver: DefaultDriver) {
    let mut harness = TestHarness::new_with_policy(&driver, VirtioConsoleDisconnectPolicy::Discard);
    harness.enable().await;
    harness.handle.set_read_error_then_disconnect();
    yield_until(|| harness.handle.disconnect_poll_count() != 0).await;

    harness.handle.reconnect();
    let gpa = harness.post_rx_buffer_and_signal(0, 64);
    harness.handle.inject_rx_data(b"after-read-error");
    let (_, used_len) = harness.wait_for_rx_used().await;
    let mut received = vec![0; used_len as usize];
    harness.mem.read_at(gpa, &mut received).unwrap();
    assert_eq!(received, b"after-read-error");
}
