#![expect(missing_docs)] // temp

mod tests;

use anyhow::Context;
use vmbus_async::async_dgram::AsyncRecvExt;
use vmbus_async::async_dgram::AsyncSendExt;
use vmbus_async::pipe::MessagePipe;
use vmbus_ring::RingMem;
use vpci_protocol::MessageType;
use vpci_protocol::ProtocolVersion;
use vpci_protocol::QueryProtocolVersion;
use vpci_protocol::QueryProtocolVersionReply;
use vpci_protocol::Status;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

pub struct VpciClient<R: RingMem> {
    pipe: VpciPipe<R>,
    protocol_version: ProtocolVersion,
}

struct VpciPipe<R: RingMem> {
    pipe: MessagePipe<R>,
    buf: Vec<u8>,
}

impl<R: RingMem> VpciPipe<R> {
    async fn recv(&mut self) -> anyhow::Result<&[u8]> {
        let bytes = self.pipe.recv(&mut self.buf).await?;
        Ok(&self.buf[..bytes])
    }

    async fn recv_prefix<T: FromBytes>(&mut self) -> anyhow::Result<T> {
        let bytes = self.recv().await?;
        Ok(T::read_from_prefix(bytes)
            .ok()
            .context("failed to parse message")?
            .0)
    }
}

async fn negotiate<R: RingMem>(pipe: &mut VpciPipe<R>) -> anyhow::Result<ProtocolVersion> {
    // Try to negotiate versions in order from newest to oldest
    let versions = &[ProtocolVersion::VB];

    for &version in versions {
        tracing::debug!(?version, "trying protocol version");

        // Create the protocol version query message
        let query = QueryProtocolVersion {
            message_type: MessageType::QUERY_PROTOCOL_VERSION,
            protocol_version: version,
        };

        // Send the query and wait for response
        pipe.pipe
            .send(query.as_bytes())
            .await
            .context("failed to send protocol version query")?;

        let reply: QueryProtocolVersionReply = pipe
            .recv_prefix()
            .await
            .context("failed to receive protocol version reply")?;

        if reply.status == Status::SUCCESS {
            return Ok(version);
        }
    }

    anyhow::bail!("no supported VPCI protocol version found");
}

impl<R: RingMem> VpciClient<R> {
    pub async fn connect(pipe: MessagePipe<R>) -> anyhow::Result<Self> {
        let mut pipe = VpciPipe {
            pipe,
            buf: vec![0; vpci_protocol::MAXIMUM_PACKET_SIZE],
        };

        let version = negotiate(&mut pipe)
            .await
            .context("failed to negotiate protocol version")?;

        Ok(Self {
            pipe,
            protocol_version: version,
        })
    }

    /// Returns the negotiated protocol version
    pub fn protocol_version(&self) -> ProtocolVersion {
        self.protocol_version
    }
}
