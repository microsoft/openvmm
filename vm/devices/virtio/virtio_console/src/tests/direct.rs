// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for input gating, read-error handling, and output retention in the
//! direct worker mode.

use super::TX_USED_ADDR;
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

/// A guest that resets the console while host input is gated, as a snapshot
/// boundary or a gated restore gates it, and then starts it again receives no
/// host input until the gate is released.
#[async_test]
async fn guest_reset_keeps_host_input_gated(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;
    harness.device.quiesce_input().await.unwrap();

    // The guest's reset, as the transport makes it when the guest writes 0
    // to the device status.
    harness.disable().await;
    harness.reset_rings();
    harness.enable().await;

    harness.handle.inject_rx_data(b"gated-input");
    let gpa = harness.post_rx_buffer_and_signal(0, 64);
    // The worker runs: it forwards the guest's output.
    harness.post_tx_and_signal(0, b"tx-while-gated");
    assert_eq!(harness.wait_for_tx_used().await, (0, 0));
    for _ in 0..10 {
        yield_now().await;
    }
    assert_eq!(harness.handle.pending_rx_len(), b"gated-input".len());

    harness.device.resume_input().await.unwrap();
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

/// The index of the transmit queue's used ring, which follows its flags.
fn tx_used_idx(harness: &TestHarness) -> u16 {
    harness.mem.read_plain(TX_USED_ADDR + 2).unwrap()
}

#[async_test]
async fn retain_keeps_output_whose_write_fails_for_the_next_peer(driver: DefaultDriver) {
    let mut harness = TestHarness::new_with_policy(&driver, VirtioConsoleDisconnectPolicy::Retain);
    harness.enable().await;
    harness.send_guest_bytes(0, b"before").await;
    assert_eq!(harness.handle.take_tx_data(), b"before");

    // The peer leaves before the worker notices, so the worker writes the
    // guest's next output to it.
    harness.handle.set_peer_departed();
    harness.post_tx_and_signal(1, b"after");
    harness.wait_for_connect().await;
    assert!(harness.handle.take_tx_data().is_empty());
    assert_eq!(tx_used_idx(&harness), 1);

    harness.handle.reconnect();
    assert_eq!(harness.wait_for_tx_used().await, (1, 0));
    assert_eq!(harness.handle.take_tx_data(), b"after");
}

#[async_test]
async fn retain_sends_the_rest_of_a_partial_write_to_the_next_peer(driver: DefaultDriver) {
    let mut harness = TestHarness::new_with_policy(&driver, VirtioConsoleDisconnectPolicy::Retain);
    harness.enable().await;
    harness.handle.set_write_limit_then_disconnect(3);

    harness.post_tx_and_signal(0, b"abcdef");
    harness.wait_for_connect().await;
    assert_eq!(harness.handle.take_tx_data(), b"abc");
    assert_eq!(tx_used_idx(&harness), 0);

    harness.handle.reconnect();
    assert_eq!(harness.wait_for_tx_used().await, (0, 0));
    assert_eq!(harness.handle.take_tx_data(), b"def");
}
