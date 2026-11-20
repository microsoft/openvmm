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

struct Worker {
    ic: KvpIc,
    channel: KvpChannel<GatedRingMem>,
}

impl AsyncRun<()> for Worker {
    async fn run(&mut self, stop: &mut StopTask<'_>, _: &mut ()) -> Result<(), Cancelled> {
        let check = stop.check_handle()?;
        stop.until_stopped(self.channel.process(&mut self.ic, &check))
            .await?
    }
}

#[async_test]
async fn stop_between_rejected_responses_keeps_kvp_negotiation(driver: DefaultDriver) {
    let (_thread, worker_driver) = pal_async::DefaultPool::spawn_on_thread("kvp-stop");
    for malformed in [false, true] {
        let (host, guest, gate) = gated_channels(16384);
        let mut guest = MessagePipe::new(guest).unwrap();
        let response = hyperv_ic_protocol::Header {
            message_type: hyperv_ic_protocol::MessageType::KVP_EXCHANGE,
            flags: HeaderFlags::new()
                .with_response(true)
                .with_transaction(true),
            ..FromZeros::new_zeroed()
        };
        guest
            .try_send(if malformed { &[0] } else { response.as_bytes() })
            .unwrap();
        guest.try_send(response.as_bytes()).unwrap();
        let (_send, recv) = mesh::channel();
        let mut task = TaskControl::new(Worker {
            ic: KvpIc::new(recv),
            channel: KvpChannel::new(host, None).unwrap(),
        });
        task.insert(&worker_driver, "kvp", ());
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
}

#[test]
fn string_round_trip_fills_buffer_exactly() {
    let mut buf = [0xffff; 4];
    // The surrogate pair and terminator exactly fill the buffer.
    let n = write_str(&mut buf, "a\u{1f680}").unwrap();
    assert_eq!(n, 8);
    assert_eq!(parse_str(&buf, n).unwrap(), "a\u{1f680}");
    assert!(write_str(&mut buf, "abcd").is_err());
}

#[test]
fn malformed_strings_are_rejected() {
    let mut buf = [0; 8];
    assert!(write_str(&mut buf, "a\0b").is_err());

    let guest = [b'a' as u16, 0, b'b' as u16, 0];
    assert_eq!(parse_str(&guest, 4).unwrap(), "a");
    // Interior null, odd length, too long, and missing terminator.
    for n in [8, 3, 10, 2] {
        assert!(parse_str(&guest, n).is_err(), "{n}");
    }
}
