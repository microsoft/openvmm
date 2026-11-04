// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use futures::FutureExt;
use pal_async::DefaultDriver;
use pal_async::async_test;
use task_control::AsyncRun;
use task_control::TaskControl;
use test_with_tracing::test;
use vmbus_async::async_dgram::AsyncRecvExt;
use vmbus_async::pipe::MessagePipe;
use vmbus_channel::test_helpers::GatedRingMem;
use vmbus_channel::test_helpers::gated_channels;
use vmbus_channel::test_helpers::stop_with_timeout;
use vmcore::reference_time::GetReferenceTime;
use vmcore::reference_time::ReferenceTimeResult;
use zerocopy::FromZeros;

struct UnusedTime;

impl GetReferenceTime for UnusedTime {
    fn now(&self) -> ReferenceTimeResult {
        panic!("time must not be sampled before negotiation completes");
    }
}

struct Worker {
    ic: TimesyncIc,
    channel: TimesyncChannel<GatedRingMem>,
}

impl AsyncRun<()> for Worker {
    async fn run(&mut self, stop: &mut StopTask<'_>, _: &mut ()) -> Result<(), Cancelled> {
        let check = stop.check_handle()?;
        stop.until_stopped(self.channel.process(&mut self.ic, &check))
            .await?
    }
}

#[async_test]
async fn stop_between_stale_responses_keeps_timesync_negotiation(driver: DefaultDriver) {
    let (_thread, worker_driver) = pal_async::DefaultPool::spawn_on_thread("timesync-stop");
    let (host, guest, gate) = gated_channels(16384);
    let mut guest = MessagePipe::new(guest).unwrap();
    let response = hyperv_ic_protocol::Header {
        message_type: hyperv_ic_protocol::MessageType::TIME_SYNC,
        flags: hyperv_ic_protocol::HeaderFlags::new()
            .with_response(true)
            .with_transaction(true),
        ..FromZeros::new_zeroed()
    };
    guest.try_send(response.as_bytes()).unwrap();
    guest.try_send(response.as_bytes()).unwrap();
    let mut task = TaskControl::new(Worker {
        ic: TimesyncIc::new(&worker_driver, ReferenceTimeSource::new(UnusedTime)),
        channel: TimesyncChannel::new(host, None).unwrap(),
    });
    task.insert(&worker_driver, "timesync", ());
    assert!(task.start());
    gate.stop(&mut task, &driver).await;
    assert!(matches!(
        &task.task().channel.state,
        ChannelState::Negotiate(NegotiateState::WaitVersion)
    ));
    let mut buffer = [0; 256];
    let n = task
        .task_mut()
        .channel
        .pipe
        .pipe
        .recv(&mut buffer)
        .now_or_never()
        .unwrap()
        .unwrap();
    assert_eq!(&buffer[..n], response.as_bytes());
    assert!(task.start());
    stop_with_timeout(&mut task, &driver).await;
}
