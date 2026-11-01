// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use futures::FutureExt;
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
