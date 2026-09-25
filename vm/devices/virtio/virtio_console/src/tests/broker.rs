// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for the control-session broker worker mode.

use super::QUEUE_SIZE;
use super::TestHarness;
use super::harness::BROKER_CAPABILITY;
use super::harness::BROKER_INSTANCE;
use super::harness::ack;
use super::harness::decode;
use super::harness::encode;
use super::harness::yield_now;
use super::harness::yield_until;
use crate::broker::HostTransportState;
use crate::broker::disconnect_host_transport;
use crate::broker::initial_host_transport_state;
use crate::control_session_protocol;
use crate::control_session_protocol::Record;
use crate::control_session_protocol::RecordType;
use pal_async::DefaultDriver;
use pal_async::async_test;
use serial_core::LocalPeerIdentity;
use serial_core::disconnected::Disconnected;
use std::time::Duration;
use test_with_tracing::test;
use virtio::VirtioDevice;

#[test]
fn disconnected_broker_transport_waits_for_connect() {
    let mut io = Disconnected;
    assert_eq!(
        initial_host_transport_state(&io),
        HostTransportState::WaitingForConnect
    );
    assert_eq!(
        disconnect_host_transport(&mut io),
        HostTransportState::WaitingForConnect
    );
}

async fn activate_broker(harness: &mut TestHarness) {
    harness
        .send_guest_bytes(
            0,
            &encode(&Record::bootstrap(RecordType::GuestAttach, Vec::new())),
        )
        .await;
    let reset = decode(&harness.receive_guest_bytes(0, 128).await);
    assert_eq!(reset.record_type, RecordType::Reset);
    assert_eq!(reset.instance_id, BROKER_INSTANCE);

    harness
        .send_guest_bytes(1, &encode(&ack(BROKER_INSTANCE, 1)))
        .await;
    harness.handle.inject_rx_data(&encode(&Record::bootstrap(
        RecordType::HostAttach,
        BROKER_CAPABILITY.to_vec(),
    )));
    yield_until(|| harness.handle.tx_data().len() >= control_session_protocol::HEADER_LEN).await;
    let ready = decode(&harness.handle.take_tx_data());
    assert_eq!(ready.record_type, RecordType::Ready);
}

#[async_test]
async fn broker_cold_attach_ready_and_bidirectional_data(driver: DefaultDriver) {
    let mut harness = TestHarness::new_broker(&driver, BROKER_INSTANCE, BROKER_CAPABILITY);
    harness.enable().await;

    harness.handle.inject_rx_data(&encode(&Record::bootstrap(
        RecordType::HostAttach,
        BROKER_CAPABILITY.to_vec(),
    )));
    yield_until(|| harness.handle.tx_data().len() >= control_session_protocol::HEADER_LEN).await;
    let wait = decode(&harness.handle.take_tx_data());
    assert_eq!(wait.record_type, RecordType::Wait);

    harness
        .send_guest_bytes(
            0,
            &encode(&Record::bootstrap(RecordType::GuestAttach, Vec::new())),
        )
        .await;
    let reset = decode(&harness.receive_guest_bytes(0, 128).await);
    assert_eq!((reset.record_type, reset.epoch), (RecordType::Reset, 1));
    harness
        .send_guest_bytes(1, &encode(&ack(BROKER_INSTANCE, 1)))
        .await;
    yield_until(|| harness.handle.tx_data().len() >= control_session_protocol::HEADER_LEN).await;
    assert_eq!(
        decode(&harness.handle.take_tx_data()).record_type,
        RecordType::Ready
    );

    harness
        .send_guest_bytes(
            2,
            &encode(&Record::session(
                RecordType::Data,
                BROKER_INSTANCE,
                1,
                1,
                b"guest-to-host".to_vec(),
            )),
        )
        .await;
    yield_until(|| harness.handle.tx_data().len() >= control_session_protocol::HEADER_LEN + 13)
        .await;
    let host_record = decode(&harness.handle.take_tx_data());
    assert_eq!(host_record.payload, b"guest-to-host");

    harness.handle.inject_rx_data(&encode(&Record::session(
        RecordType::Data,
        BROKER_INSTANCE,
        1,
        0,
        b"host-to-guest".to_vec(),
    )));
    let guest_record = decode(&harness.receive_guest_bytes(1, 128).await);
    assert_eq!(guest_record.payload, b"host-to-guest");
}

#[async_test]
async fn broker_rejects_wrong_identity_before_reserving_host_slot(driver: DefaultDriver) {
    let mut harness = TestHarness::new_broker(&driver, BROKER_INSTANCE, BROKER_CAPABILITY);
    harness
        .handle
        .set_peer_identity(Some(LocalPeerIdentity::UnixUid(1001)));
    harness.enable().await;
    yield_until(|| !harness.handle.is_connected()).await;

    harness
        .send_guest_bytes(
            0,
            &encode(&Record::bootstrap(RecordType::GuestAttach, Vec::new())),
        )
        .await;
    let reset = decode(&harness.receive_guest_bytes(0, 128).await);
    assert_eq!((reset.record_type, reset.epoch), (RecordType::Reset, 1));

    harness
        .handle
        .set_peer_identity(Some(LocalPeerIdentity::UnixUid(1000)));
    harness.handle.reconnect();
    harness.handle.inject_rx_data(&encode(&Record::bootstrap(
        RecordType::HostAttach,
        BROKER_CAPABILITY.to_vec(),
    )));
    yield_until(|| harness.handle.tx_data().len() >= control_session_protocol::HEADER_LEN).await;
    let wait = decode(&harness.handle.take_tx_data());
    assert_eq!((wait.record_type, wait.epoch), (RecordType::Wait, 1));
}

#[async_test]
async fn broker_rejects_wrong_capability_without_epoch_advance(driver: DefaultDriver) {
    let mut harness = TestHarness::new_broker(&driver, BROKER_INSTANCE, BROKER_CAPABILITY);
    harness.enable().await;
    harness.handle.inject_rx_data(&encode(&Record::bootstrap(
        RecordType::HostAttach,
        [0x7d; 32].to_vec(),
    )));
    yield_until(|| !harness.handle.is_connected()).await;
    assert!(harness.handle.tx_data().is_empty());

    harness
        .send_guest_bytes(
            0,
            &encode(&Record::bootstrap(RecordType::GuestAttach, Vec::new())),
        )
        .await;
    let reset = decode(&harness.receive_guest_bytes(0, 128).await);
    assert_eq!((reset.record_type, reset.epoch), (RecordType::Reset, 1));

    harness.handle.reconnect();
    harness.handle.inject_rx_data(&encode(&Record::bootstrap(
        RecordType::HostAttach,
        BROKER_CAPABILITY.to_vec(),
    )));
    yield_until(|| harness.handle.tx_data().len() >= control_session_protocol::HEADER_LEN).await;
    let wait = decode(&harness.handle.take_tx_data());
    assert_eq!((wait.record_type, wait.epoch), (RecordType::Wait, 1));
}

#[async_test]
async fn broker_auth_timeout_frees_slot_without_epoch_advance(driver: DefaultDriver) {
    let mut harness =
        TestHarness::new_broker_with_timeout(&driver, BROKER_INSTANCE, BROKER_CAPABILITY, 5);
    harness.enable().await;
    let mut timer = pal_async::timer::PolledTimer::new(&driver);
    timer.sleep(Duration::from_millis(20)).await;
    yield_until(|| !harness.handle.is_connected()).await;

    harness
        .send_guest_bytes(
            0,
            &encode(&Record::bootstrap(RecordType::GuestAttach, Vec::new())),
        )
        .await;
    let reset = decode(&harness.receive_guest_bytes(0, 128).await);
    assert_eq!((reset.record_type, reset.epoch), (RecordType::Reset, 1));

    harness.handle.reconnect();
    harness.handle.inject_rx_data(&encode(&Record::bootstrap(
        RecordType::HostAttach,
        BROKER_CAPABILITY.to_vec(),
    )));
    yield_until(|| harness.handle.tx_data().len() >= control_session_protocol::HEADER_LEN).await;
    let wait = decode(&harness.handle.take_tx_data());
    assert_eq!((wait.record_type, wait.epoch), (RecordType::Wait, 1));
}

#[async_test]
async fn broker_quiesce_stops_new_host_input(driver: DefaultDriver) {
    let mut harness = TestHarness::new_broker(&driver, BROKER_INSTANCE, BROKER_CAPABILITY);
    harness.enable().await;
    activate_broker(&mut harness).await;

    harness.device.quiesce_input().await.unwrap();
    let host_data = encode(&Record::session(
        RecordType::Data,
        BROKER_INSTANCE,
        1,
        0,
        b"after-quiesce".to_vec(),
    ));
    harness.handle.inject_rx_data(&host_data);
    for _ in 0..20 {
        yield_now().await;
    }
    assert_eq!(harness.handle.pending_rx_len(), host_data.len());

    harness.device.resume_input().await.unwrap();
    yield_until(|| harness.handle.pending_rx_len() == 0).await;
    let record = decode(
        &harness
            .receive_guest_bytes(3, control_session_protocol::HEADER_LEN as u32 + 32)
            .await,
    );
    assert_eq!(
        (record.record_type, record.payload),
        (RecordType::Data, b"after-quiesce".to_vec())
    );
}

#[async_test]
async fn broker_host_lifecycle_error_does_not_stop_guest_worker(driver: DefaultDriver) {
    let mut harness = TestHarness::new_broker(&driver, BROKER_INSTANCE, BROKER_CAPABILITY);
    harness.enable().await;
    activate_broker(&mut harness).await;

    harness.handle.set_disconnect_error();
    let reset = decode(
        &harness
            .receive_guest_bytes(3, control_session_protocol::HEADER_LEN as u32)
            .await,
    );
    assert_eq!((reset.record_type, reset.epoch), (RecordType::Reset, 2));
    harness
        .send_guest_bytes(4, &encode(&ack(BROKER_INSTANCE, 2)))
        .await;
    assert!(harness.device.stop_queue(0).await.is_some());
    assert!(harness.device.stop_queue(1).await.is_some());
    let (worker, state) = harness.device.worker.get();
    let crate::direct::ConsoleWorkerMode::Broker(mode) = &worker.mode else {
        panic!("expected broker worker");
    };
    assert_eq!(
        mode.broker.state(),
        crate::control_session_broker::BrokerState::ReadyNoHost
    );
    assert!(state.is_some());
}

#[async_test]
async fn broker_disconnect_emits_reset_without_guest_eof(driver: DefaultDriver) {
    let mut harness = TestHarness::new_broker(&driver, BROKER_INSTANCE, BROKER_CAPABILITY);
    harness.enable().await;
    activate_broker(&mut harness).await;

    harness.handle.disconnect();
    let reset = decode(&harness.receive_guest_bytes(1, 128).await);
    assert_eq!(
        (reset.record_type, reset.instance_id, reset.epoch),
        (RecordType::Reset, BROKER_INSTANCE, 2)
    );

    harness.handle.reconnect();
    harness.handle.inject_rx_data(&encode(&Record::bootstrap(
        RecordType::HostAttach,
        BROKER_CAPABILITY.to_vec(),
    )));
    yield_until(|| harness.handle.tx_data().len() >= control_session_protocol::HEADER_LEN).await;
    let wait = decode(&harness.handle.take_tx_data());
    assert_eq!((wait.record_type, wait.epoch), (RecordType::Wait, 2));
}

#[async_test]
async fn broker_zero_credit_disconnects_and_reconnects_without_guest_release(
    driver: DefaultDriver,
) {
    let mut harness = TestHarness::new_broker(&driver, BROKER_INSTANCE, BROKER_CAPABILITY);
    harness.enable().await;
    activate_broker(&mut harness).await;

    let full_host_record = encode(&Record::session(
        RecordType::Data,
        BROKER_INSTANCE,
        1,
        0,
        vec![0x5a; control_session_protocol::MAX_DATA_LEN],
    ));
    let full_guest_record = encode(&Record::session(
        RecordType::Data,
        BROKER_INSTANCE,
        1,
        1,
        vec![0x5a; control_session_protocol::MAX_DATA_LEN],
    ));
    harness.handle.inject_rx_data(&full_host_record);
    assert_eq!(
        harness
            .receive_guest_exact(2, full_guest_record.len())
            .await,
        full_guest_record
    );

    let blocked = encode(&Record::session(
        RecordType::Data,
        BROKER_INSTANCE,
        1,
        1,
        vec![0x44],
    ));
    harness.handle.inject_rx_data(&blocked);
    yield_until(|| harness.handle.pending_rx_len() == 0).await;

    harness.handle.disconnect();
    let reset = decode(
        &harness
            .receive_guest_exact(4, control_session_protocol::HEADER_LEN)
            .await,
    );
    assert_eq!((reset.record_type, reset.epoch), (RecordType::Reset, 2));

    harness.handle.reconnect();
    harness.handle.inject_rx_data(&encode(&Record::bootstrap(
        RecordType::HostAttach,
        BROKER_CAPABILITY.to_vec(),
    )));
    yield_until(|| harness.handle.tx_data().len() >= control_session_protocol::HEADER_LEN).await;
    assert_eq!(
        decode(&harness.handle.take_tx_data()).record_type,
        RecordType::Wait
    );
    harness
        .send_guest_bytes(5, &encode(&ack(BROKER_INSTANCE, 2)))
        .await;
    yield_until(|| harness.handle.tx_data().len() >= control_session_protocol::HEADER_LEN).await;
    assert_eq!(
        decode(&harness.handle.take_tx_data()).record_type,
        RecordType::Ready
    );
}

#[async_test]
async fn broker_disconnect_finishes_partial_data_before_reset(driver: DefaultDriver) {
    let mut harness = TestHarness::new_broker(&driver, BROKER_INSTANCE, BROKER_CAPABILITY);
    harness.enable().await;
    activate_broker(&mut harness).await;

    let host_data = encode(&Record::session(
        RecordType::Data,
        BROKER_INSTANCE,
        1,
        0,
        b"partially-emitted".to_vec(),
    ));
    let guest_data = encode(&Record::session(
        RecordType::Data,
        BROKER_INSTANCE,
        1,
        1,
        b"partially-emitted".to_vec(),
    ));
    harness.handle.inject_rx_data(&host_data);
    let first = harness.receive_guest_bytes(2, 13).await;
    assert_eq!(first, guest_data[..13]);

    harness.handle.disconnect();
    let remainder = harness.receive_guest_exact(3, guest_data.len() - 13).await;
    let mut completed = first;
    completed.extend(remainder);
    assert_eq!(completed, guest_data);
    assert_eq!(
        decode(
            &harness
                .receive_guest_exact(5, control_session_protocol::HEADER_LEN)
                .await,
        )
        .record_type,
        RecordType::Reset
    );
}

#[async_test]
async fn broker_initial_device_reset_preserves_connected_host(driver: DefaultDriver) {
    let mut harness = TestHarness::new_broker(&driver, BROKER_INSTANCE, BROKER_CAPABILITY);
    assert!(harness.handle.is_connected());

    harness.device.reset().await;
    {
        let (worker, _) = harness.device.worker.get();
        let mode = match &worker.mode {
            crate::direct::ConsoleWorkerMode::Broker(mode) => mode,
            _ => unreachable!(),
        };
        assert_eq!(
            mode.broker.state(),
            crate::control_session_broker::BrokerState::AwaitGuestAttach
        );
        assert!(!mode.broker.host_is_connected());
    }
    assert!(harness.handle.is_connected());

    harness.enable().await;
    harness.handle.inject_rx_data(&encode(&Record::bootstrap(
        RecordType::HostAttach,
        BROKER_CAPABILITY.to_vec(),
    )));
    yield_until(|| harness.handle.tx_data().len() >= control_session_protocol::HEADER_LEN).await;
    assert_eq!(
        decode(&harness.handle.take_tx_data()).record_type,
        RecordType::Wait
    );
    assert!(harness.handle.is_connected());
}

#[async_test]
async fn broker_device_reset_keeps_quarantined_host_in_waiting_for_disconnect(
    driver: DefaultDriver,
) {
    let mut harness = TestHarness::new_broker(&driver, BROKER_INSTANCE, BROKER_CAPABILITY);
    harness
        .handle
        .set_peer_identity(Some(LocalPeerIdentity::UnixUid(4242)));
    harness.handle.set_disconnect_current_error();
    harness.enable().await;

    yield_until(|| harness.handle.disconnect_poll_count() > 0).await;
    let receive_state = harness.device.stop_queue(0).await;
    let transmit_state = harness.device.stop_queue(1).await;
    {
        let (worker, _) = harness.device.worker.get();
        let mode = match &worker.mode {
            crate::direct::ConsoleWorkerMode::Broker(mode) => mode,
            _ => unreachable!(),
        };
        assert_eq!(
            mode.broker.state(),
            crate::control_session_broker::BrokerState::AwaitGuestAttach
        );
        assert!(!mode.broker.host_is_connected());
        assert_eq!(
            mode.transport_state,
            HostTransportState::WaitingForDisconnect
        );
    }
    assert!(harness.handle.is_connected());

    harness.device.reset().await;
    {
        let (worker, _) = harness.device.worker.get();
        let mode = match &worker.mode {
            crate::direct::ConsoleWorkerMode::Broker(mode) => mode,
            _ => unreachable!(),
        };
        assert_eq!(
            mode.broker.state(),
            crate::control_session_broker::BrokerState::AwaitGuestAttach
        );
        assert!(!mode.broker.host_is_connected());
        assert_eq!(
            mode.transport_state,
            HostTransportState::WaitingForDisconnect
        );
    }
    assert!(harness.handle.is_connected());

    harness
        .enable_with_state(receive_state, transmit_state)
        .await;
    harness
        .handle
        .set_peer_identity(Some(LocalPeerIdentity::UnixUid(1000)));
    let host_attach = encode(&Record::bootstrap(
        RecordType::HostAttach,
        BROKER_CAPABILITY.to_vec(),
    ));
    harness.handle.inject_rx_data(&host_attach);
    for _ in 0..20 {
        yield_now().await;
    }
    assert_eq!(harness.handle.pending_rx_len(), host_attach.len());
    assert!(harness.handle.tx_data().is_empty());

    let disconnect_polls_before = harness.handle.disconnect_poll_count();
    harness.handle.disconnect();
    yield_until(|| harness.handle.disconnect_poll_count() > disconnect_polls_before).await;
    harness.handle.reconnect();
    yield_until(|| harness.handle.tx_data().len() >= control_session_protocol::HEADER_LEN).await;
    assert_eq!(
        decode(&harness.handle.take_tx_data()).record_type,
        RecordType::Wait
    );
}

#[async_test]
async fn broker_device_reset_requires_a_fresh_host_attachment(driver: DefaultDriver) {
    let mut harness = TestHarness::new_broker(&driver, BROKER_INSTANCE, BROKER_CAPABILITY);
    harness.enable().await;
    activate_broker(&mut harness).await;

    harness.device.stop_queue(0).await;
    harness.device.stop_queue(1).await;
    harness.device.reset().await;
    {
        let (worker, _) = harness.device.worker.get();
        let mode = match &worker.mode {
            crate::direct::ConsoleWorkerMode::Broker(mode) => mode,
            _ => unreachable!(),
        };
        assert_eq!(
            mode.broker.state(),
            crate::control_session_broker::BrokerState::AwaitGuestAttach
        );
        assert!(!mode.broker.host_is_connected());
    }

    harness.handle.disconnect();
    harness.reset_rings();
    harness.enable().await;
    harness
        .send_guest_bytes(
            0,
            &encode(&Record::bootstrap(RecordType::GuestAttach, Vec::new())),
        )
        .await;
    let reset = decode(&harness.receive_guest_bytes(0, 128).await);
    assert_eq!(
        (reset.record_type, reset.instance_id, reset.epoch),
        (RecordType::Reset, BROKER_INSTANCE, 1)
    );
}

#[async_test]
async fn broker_fragmentation_partial_host_writes_and_backpressure(driver: DefaultDriver) {
    let mut harness = TestHarness::new_broker(&driver, BROKER_INSTANCE, BROKER_CAPABILITY);
    harness.enable().await;

    let guest_attach = encode(&Record::bootstrap(RecordType::GuestAttach, Vec::new()));
    harness.send_guest_bytes(0, &guest_attach[..17]).await;
    harness.send_guest_bytes(1, &guest_attach[17..]).await;
    assert_eq!(
        decode(&harness.receive_guest_bytes(0, 128).await).record_type,
        RecordType::Reset
    );
    harness
        .send_guest_bytes(2, &encode(&ack(BROKER_INSTANCE, 1)))
        .await;

    harness.handle.set_max_write_size(3);
    let host_attach = encode(&Record::bootstrap(
        RecordType::HostAttach,
        BROKER_CAPABILITY.to_vec(),
    ));
    harness.handle.inject_rx_data(&host_attach[..11]);
    for _ in 0..5 {
        yield_now().await;
    }
    harness.handle.inject_rx_data(&host_attach[11..]);
    yield_until(|| harness.handle.tx_data().len() >= control_session_protocol::HEADER_LEN).await;
    assert_eq!(
        decode(&harness.handle.take_tx_data()).record_type,
        RecordType::Ready
    );

    harness.handle.set_write_blocked(true);
    for sequence in 1..=18 {
        harness
            .send_guest_bytes(
                ((sequence + 2) % u64::from(QUEUE_SIZE)) as u16,
                &encode(&Record::session(
                    RecordType::Data,
                    BROKER_INSTANCE,
                    1,
                    sequence,
                    vec![sequence as u8],
                )),
            )
            .await;
    }
    assert!(harness.handle.tx_data().is_empty());
    harness.handle.set_write_blocked(false);
    let expected = 18 * (control_session_protocol::HEADER_LEN + 1);
    yield_until(|| harness.handle.tx_data().len() >= expected).await;
    let bytes = harness.handle.take_tx_data();
    let mut offset = 0;
    for sequence in 1..=18 {
        let end = offset + control_session_protocol::HEADER_LEN + 1;
        let record = decode(&bytes[offset..end]);
        assert_eq!(
            (record.sequence, record.payload),
            (sequence, vec![sequence as u8])
        );
        offset = end;
    }
    assert_eq!(offset, bytes.len());
}

#[async_test]
async fn broker_restore_finishes_old_output_then_uses_fresh_identity(driver: DefaultDriver) {
    let mut harness = TestHarness::new_broker(&driver, BROKER_INSTANCE, BROKER_CAPABILITY);
    harness.enable().await;
    activate_broker(&mut harness).await;

    let old_data = encode(&Record::session(
        RecordType::Data,
        BROKER_INSTANCE,
        1,
        0,
        b"partially-emitted".to_vec(),
    ));
    harness.handle.inject_rx_data(&old_data);
    let first = harness.receive_guest_bytes(1, 13).await;
    assert_eq!(first, old_data[..13]);

    let old_guest_data = encode(&Record::session(
        RecordType::Data,
        BROKER_INSTANCE,
        1,
        1,
        b"partially-parsed".to_vec(),
    ));
    harness.send_guest_bytes(2, &old_guest_data[..19]).await;

    let receive_state = harness.device.stop_queue(0).await.unwrap();
    let transmit_state = harness.device.stop_queue(1).await.unwrap();
    let saved = harness.device.save_device().unwrap().unwrap();

    const NEW_INSTANCE: [u8; 16] = [0x62; 16];
    const NEW_CAPABILITY: [u8; 32] = [0xb8; 32];
    harness.replace_with_broker(NEW_INSTANCE, NEW_CAPABILITY);
    harness.device.restore_device(Some(saved)).unwrap();
    assert!(!harness.handle.is_connected());
    {
        let (worker, _) = harness.device.worker.get();
        let crate::direct::ConsoleWorkerMode::Broker(mode) = &worker.mode else {
            panic!("expected broker worker");
        };
        assert_eq!(
            (
                mode.broker.guest_receive_window(),
                mode.broker.guest_receive_credit()
            ),
            (0, 0)
        );
    }
    harness
        .enable_with_state(Some(receive_state), Some(transmit_state))
        .await;

    let remainder = harness.receive_guest_bytes(2, 128).await;
    let mut completed_old_output = first;
    completed_old_output.extend(remainder);
    let completed_old_output = decode(&completed_old_output);
    assert_eq!(completed_old_output.payload, b"partially-emitted");
    assert_eq!(completed_old_output.instance_id, BROKER_INSTANCE);
    let new_reset = decode(&harness.receive_guest_bytes(3, 128).await);
    assert_eq!(
        (
            new_reset.record_type,
            new_reset.instance_id,
            new_reset.epoch
        ),
        (RecordType::Reset, NEW_INSTANCE, 1)
    );

    harness.handle.reconnect();
    harness.handle.inject_rx_data(&encode(&Record::bootstrap(
        RecordType::HostAttach,
        BROKER_CAPABILITY.to_vec(),
    )));
    yield_until(|| !harness.handle.is_connected()).await;
    harness.handle.reconnect();
    harness.handle.inject_rx_data(&encode(&Record::bootstrap(
        RecordType::HostAttach,
        NEW_CAPABILITY.to_vec(),
    )));
    yield_until(|| harness.handle.tx_data().len() >= control_session_protocol::HEADER_LEN).await;
    let wait = decode(&harness.handle.take_tx_data());
    assert_eq!(
        (wait.record_type, wait.instance_id, wait.epoch),
        (RecordType::Wait, NEW_INSTANCE, 1)
    );

    harness.send_guest_bytes(3, &old_guest_data[19..]).await;
    harness
        .send_guest_bytes(4, &encode(&ack(NEW_INSTANCE, 1)))
        .await;
}

#[async_test]
async fn malformed_host_input_detaches_without_panicking(driver: DefaultDriver) {
    let mut harness = TestHarness::new_broker(&driver, BROKER_INSTANCE, BROKER_CAPABILITY);
    harness.enable().await;
    harness
        .handle
        .inject_rx_data(&[0x99; control_session_protocol::HEADER_LEN]);
    for _ in 0..20 {
        yield_now().await;
    }
    yield_until(|| !harness.handle.is_connected()).await;
    harness.handle.reconnect();
    harness.handle.inject_rx_data(&encode(&Record::bootstrap(
        RecordType::HostAttach,
        BROKER_CAPABILITY.to_vec(),
    )));
    yield_until(|| harness.handle.tx_data().len() >= control_session_protocol::HEADER_LEN).await;
    assert_eq!(
        decode(&harness.handle.take_tx_data()).record_type,
        RecordType::Wait
    );
}

#[async_test]
async fn malformed_guest_input_recovers_after_device_reset(driver: DefaultDriver) {
    let mut harness = TestHarness::new_broker(&driver, BROKER_INSTANCE, BROKER_CAPABILITY);
    harness.enable().await;
    activate_broker(&mut harness).await;
    harness.post_tx_and_signal(2, &[0x99; control_session_protocol::HEADER_LEN]);
    for _ in 0..20 {
        yield_now().await;
    }

    assert!(harness.device.stop_queue(0).await.is_some());
    assert!(harness.device.stop_queue(1).await.is_some());
    assert!(!harness.handle.is_connected());
    assert!(harness.handle.tx_data().is_empty());

    harness.device.reset().await;
    harness.handle.reconnect();
    harness.reset_rings();
    harness.enable().await;
    activate_broker(&mut harness).await;
}
