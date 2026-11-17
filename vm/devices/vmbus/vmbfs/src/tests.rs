// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use futures::FutureExt;
use pal_async::DefaultDriver;
use pal_async::async_test;
use task_control::AsyncRun;
use task_control::TaskControl;
use test_with_tracing::test;
use vmbus_async::pipe::MessagePipe;
use vmbus_channel::test_helpers::GatedRingMem;
use vmbus_channel::test_helpers::gated_channels;
use vmbus_channel::test_helpers::stop_with_timeout;
use vmbus_channel::test_helpers::with_timeout;

struct Worker {
    device: VmbfsDevice,
    channel: VmbfsChannel<GatedRingMem>,
}

impl AsyncRun<()> for Worker {
    async fn run(&mut self, stop: &mut StopTask<'_>, _: &mut ()) -> Result<(), Cancelled> {
        let check = stop.check_handle()?;
        stop.until_stopped(self.channel.process(&mut self.device, &check))
            .await?
    }
}

#[async_test]
async fn stop_bounds_ready_requests_and_restart_keeps_negotiation(driver: DefaultDriver) {
    let (_thread, worker_driver) = pal_async::DefaultPool::spawn_on_thread("vmbfs-stop");
    let (host, guest, gate) = gated_channels(16384);
    let mut guest = MessagePipe::new(guest).unwrap();
    let header = protocol::MessageHeader {
        message_type: protocol::MessageType::VERSION_REQUEST,
        reserved: 0,
    };
    let request = protocol::VersionRequest {
        requested_version: protocol::Version(0),
    };
    let packet = [header.as_bytes(), request.as_bytes()].concat();
    guest.try_send(&packet).unwrap();
    guest.try_send(&packet).unwrap();
    let backing =
        single_file_backing::VmbfsSingleFileBacking::new("test", tempfile::tempfile().unwrap())
            .unwrap();
    let mut task = TaskControl::new(Worker {
        device: VmbfsDevice::new(Box::new(backing)),
        channel: VmbfsChannel {
            state: State::VersionRequest,
            pipe: MessagePipe::new(host).unwrap(),
            buf: vec![0; protocol::MAX_MESSAGE_SIZE],
        },
    });
    task.insert(&worker_driver, "vmbfs", ());
    assert!(task.start());
    gate.stop(&mut task, &driver).await;
    let mut buffer = [0; 256];
    let n = with_timeout(&driver, guest.recv(&mut buffer))
        .await
        .unwrap();
    let (header, body) = protocol::MessageHeader::read_from_prefix(&buffer[..n]).unwrap();
    assert_eq!(header.message_type, protocol::MessageType::VERSION_RESPONSE);
    let (response, _) = protocol::VersionResponse::read_from_prefix(body).unwrap();
    assert_eq!(response.status, protocol::VersionStatus::UNSUPPORTED);
    assert!(guest.recv(&mut buffer).now_or_never().is_none());
    assert!(matches!(&task.task().channel.state, State::VersionRequest));
    assert!(task.start());
    with_timeout(&driver, guest.recv(&mut buffer))
        .await
        .unwrap();
    stop_with_timeout(&mut task, &driver).await;
    assert!(guest.recv(&mut buffer).now_or_never().is_none());
}
