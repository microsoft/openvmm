#![cfg(test)]

use mesh_node::local_node::HandlePortEvent;
use mesh_node::local_node::Port;
use mesh_node::message::Message;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;

#[cfg_attr(not(any(windows, target_os = "linux")), expect(dead_code))]
pub(crate) async fn test_message_sizes(left: Port, right: Port, range: RangeInclusive<usize>) {
    let counter = Arc::new(Counter {
        count: AtomicUsize::new(*range.start()),
        event: event_listener::Event::new(),
    });
    let _right = right.set_handler(CountHandler(counter.clone()));
    let buf = vec![0; *range.end()];
    for i in range {
        if i % 0x1000 == 0 {
            tracing::info!(i, "at message");
        }
        left.send(Message::serialized(&buf[..i], Vec::new()));
        loop {
            let listener = counter.event.listen();
            if counter.count.load(Relaxed) == i + 1 {
                break;
            }
            listener.await;
        }
    }
}

struct Counter {
    count: AtomicUsize,
    event: event_listener::Event,
}

struct CountHandler(Arc<Counter>);

impl HandlePortEvent for CountHandler {
    fn message<'a>(
        &mut self,
        _control: &mut mesh_node::local_node::PortControl<'_, 'a>,
        _message: Message<'a>,
    ) -> Result<(), mesh_node::local_node::HandleMessageError> {
        self.0.count.fetch_add(1, Relaxed);
        self.0.event.notify(1);
        Ok(())
    }

    fn close(&mut self, _control: &mut mesh_node::local_node::PortControl<'_, '_>) {}

    fn fail(
        &mut self,
        _control: &mut mesh_node::local_node::PortControl<'_, '_>,
        _err: mesh_node::local_node::NodeError,
    ) {
        panic!("failed after {} messages", self.0.count.load(Relaxed));
    }

    fn drain(&mut self) -> Vec<mesh_node::message::OwnedMessage> {
        Vec::new()
    }
}
