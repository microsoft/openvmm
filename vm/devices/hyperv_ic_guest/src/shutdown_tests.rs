// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use futures::FutureExt;
use futures::StreamExt;
use pal_async::DefaultDriver;
use pal_async::async_test;
use task_control::AsyncRun;
use task_control::TaskControl;
use test_with_tracing::test;
use vmbus_channel::test_helpers::GatedRingMem;
use vmbus_channel::test_helpers::gated_channels;
use vmbus_channel::test_helpers::stop_with_timeout;
use vmbus_channel::test_helpers::with_timeout;

struct Worker {
    ic: ShutdownGuestIc,
    channel: ShutdownGuestChannel<GatedRingMem>,
}

#[async_test]
async fn shutdown_text_is_preserved_or_rejected(driver: DefaultDriver) {
    let (guest, host) = vmbus_channel::connected_async_channels(16384);
    let mut host = MessagePipe::new(host).unwrap();
    let mut ic = ShutdownGuestIc::new();
    let mut notifications = ic.get_shutdown_notifier();
    let mut channel = ShutdownGuestChannel::new(MessagePipe::new(guest).unwrap());
    channel.state = ShutdownGuestChannelState::Running {
        framework_version: FRAMEWORK_VERSION_3,
        message_version: SHUTDOWN_VERSION_3_2,
    };
    let text = "\u{1f680}".repeat(511) + "a";
    for (units, expected) in [
        (vec![0], Some(String::new())),
        (text.encode_utf16().chain([0]).collect(), Some(text.clone())),
        (vec![b'a' as u16; 1024], None),
        (vec![0xd800, 0], None),
        (vec![0xdc00, 0], None),
        (vec![0, 0xd800], Some(String::new())),
        (vec![0], Some(String::new())),
    ] {
        let header = hyperv_ic_protocol::Header {
            message_type: hyperv_ic_protocol::MessageType::SHUTDOWN,
            message_size: size_of::<hyperv_ic_protocol::shutdown::ShutdownMessage>() as u16,
            transaction_id: 17,
            flags: hyperv_ic_protocol::HeaderFlags::new()
                .with_request(true)
                .with_transaction(true),
            ..FromZeros::new_zeroed()
        };
        let mut message = hyperv_ic_protocol::shutdown::ShutdownMessage {
            reason_code: 0,
            timeout_secs: 0,
            flags: hyperv_ic_protocol::shutdown::ShutdownFlags::new()
                .with_force(true)
                .with_restart(true),
            message: [0; 2048],
        };
        for (bytes, unit) in message.message.as_chunks_mut::<2>().0.iter_mut().zip(units) {
            bytes.copy_from_slice(&unit.to_le_bytes());
        }
        let valid = expected.is_some();
        let (result, ()) = with_timeout(&driver, async {
            futures::join!(
                channel.handle_shutdown_notification(&header, message.as_bytes(), &ic),
                async {
                    if let Some(expected) = expected {
                        let (params, completion) = notifications.next().await.unwrap().split();
                        assert_eq!(params.message.to_string(), expected);
                        assert!(params.force);
                        assert!(matches!(params.shutdown_type, ShutdownType::Reboot));
                        completion.complete(ShutdownResult::Ok);
                    }
                }
            )
        })
        .await;
        result.unwrap();
        let mut buffer = [0; 256];
        let size = with_timeout(&driver, host.recv(&mut buffer)).await.unwrap();
        let response = hyperv_ic_protocol::Header::read_from_bytes(&buffer[..size]).unwrap();
        assert_eq!(response.transaction_id, 17);
        assert_eq!(
            response.status,
            if valid {
                Status::SUCCESS
            } else {
                Status::INVALID_ARGUMENT
            }
        );
        assert!(notifications.next().now_or_never().is_none());
    }
}

impl AsyncRun<()> for Worker {
    async fn run(&mut self, stop: &mut StopTask<'_>, _: &mut ()) -> Result<(), Cancelled> {
        let check = stop.check_handle()?;
        match stop
            .until_stopped(self.channel.process(&mut self.ic, &check))
            .await?
        {
            Err(Error::Cancelled(cancelled)) => Err(cancelled),
            result => panic!("unexpected shutdown guest result: {result:?}"),
        }
    }
}

#[async_test]
async fn stop_bounds_ready_negotiation_and_resume_preserves_versions(driver: DefaultDriver) {
    let (_thread, worker_driver) = pal_async::DefaultPool::spawn_on_thread("shutdown-guest-stop");
    let (guest, host, gate) = gated_channels(16384);
    let mut host = MessagePipe::new(host).unwrap();
    let versions = [FRAMEWORK_VERSION_3, SHUTDOWN_VERSION_3];
    let message = hyperv_ic_protocol::NegotiateMessage {
        framework_version_count: 1,
        message_version_count: 1,
        ..FromZeros::new_zeroed()
    };
    let header = hyperv_ic_protocol::Header {
        message_type: hyperv_ic_protocol::MessageType::VERSION_NEGOTIATION,
        message_size: (size_of_val(&message) + size_of_val(&versions)) as u16,
        flags: hyperv_ic_protocol::HeaderFlags::new()
            .with_request(true)
            .with_transaction(true),
        ..FromZeros::new_zeroed()
    };
    let packet = [header.as_bytes(), message.as_bytes(), versions.as_bytes()].concat();
    host.try_send(&packet).unwrap();
    host.try_send(&packet).unwrap();
    let mut task = TaskControl::new(Worker {
        ic: ShutdownGuestIc::new(),
        channel: ShutdownGuestChannel::new(MessagePipe::new(guest).unwrap()),
    });
    task.insert(&worker_driver, "shutdown-guest", ());
    assert!(task.start());
    gate.stop(&mut task, &driver).await;
    assert!(matches!(
        &task.task().channel.state,
        ShutdownGuestChannelState::Running { framework_version, message_version }
        if *framework_version == FRAMEWORK_VERSION_3 && *message_version == SHUTDOWN_VERSION_3
    ));
    let mut buffer = [0; 256];
    with_timeout(&driver, host.recv(&mut buffer)).await.unwrap();
    assert!(host.recv(&mut buffer).now_or_never().is_none());
    assert!(task.start());
    with_timeout(&driver, host.recv(&mut buffer)).await.unwrap();
    stop_with_timeout(&mut task, &driver).await;
    assert!(host.recv(&mut buffer).now_or_never().is_none());
}
