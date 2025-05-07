#![expect(missing_docs)] // temp

use vmbus_async::pipe::MessagePipe;
use vmbus_ring::RingMem;

pub struct VpciClient<R: RingMem> {
    pipe: MessagePipe<R>,
}

impl<R: RingMem> VpciClient<R> {
    pub async fn connect(pipe: MessagePipe<R>) -> anyhow::Result<Self> {
        // TODO: send vpci_protocol messages to negotiate a specific version.
        // Just support the latest version for now.
    }
}
