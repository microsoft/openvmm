// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use futures::FutureExt;
use pal_async::DefaultDriver;
use pal_async::async_test;
use serial_core::disconnected::Disconnected;
use task_control::AsyncRun;
use task_control::TaskControl;
use test_with_tracing::test;
use vmbus_channel::test_helpers::GatedRingMem;
use vmbus_channel::test_helpers::gated_channels;
use vmbus_channel::test_helpers::stop_with_timeout;
use vmbus_channel::test_helpers::with_timeout;

struct Worker {
    serial: Serial,
    channel: SerialChannel<GatedRingMem>,
}

impl AsyncRun<()> for Worker {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        _: &mut (),
    ) -> Result<(), task_control::Cancelled> {
        let check = stop.check_handle()?;
        match stop
            .until_stopped(self.channel.process(&mut self.serial, &check))
            .await?
        {
            Err(Error::Cancelled(cancelled)) => Err(cancelled),
            result => panic!("unexpected serial worker result: {result:?}"),
        }
    }
}

#[async_test]
async fn stop_after_negotiation_preserves_channel_state(driver: DefaultDriver) {
    let (_thread, worker_driver) = pal_async::DefaultPool::spawn_on_thread("serial-stop");
    let (host, guest, gate) = gated_channels(16384);
    let mut guest = MessagePipe::new(guest).unwrap();
    let request = protocol::VersionRequestMessage {
        header: protocol::Header::new_host_request(HostRequests::VERSION),
        requested_version: protocol::ProtocolVersions::MANGANESE,
    };
    guest.try_send(request.as_bytes()).unwrap();
    guest.try_send(request.as_bytes()).unwrap();
    let mut task = TaskControl::new(Worker {
        serial: Serial::new(Port::Com1, Box::new(Disconnected)),
        channel: SerialChannel::new(MessagePipe::new(host).unwrap()),
    });
    task.insert(&worker_driver, "serial", ());
    assert!(task.start());
    gate.stop(&mut task, &driver).await;
    assert!(matches!(
        &task.task().channel.protocol,
        ProtocolState::Ready
    ));
    assert!(task.task().channel.state.pending_modem_status);
    let mut buffer = [0; 256];
    let n = with_timeout(&driver, guest.recv(&mut buffer))
        .await
        .unwrap();
    let (response, _) = protocol::VersionRequestResponse::read_from_prefix(&buffer[..n]).unwrap();
    assert_eq!(response.version_accepted, 1);
    assert!(guest.recv(&mut buffer).now_or_never().is_none());
    let n = task
        .task_mut()
        .channel
        .channel
        .recv(&mut buffer)
        .now_or_never()
        .unwrap()
        .unwrap();
    assert_eq!(&buffer[..n], request.as_bytes());
    assert!(task.start());
    stop_with_timeout(&mut task, &driver).await;
    assert!(matches!(
        &task.task().channel.protocol,
        ProtocolState::Ready
    ));
}
