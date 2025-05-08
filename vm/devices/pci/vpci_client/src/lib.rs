#![expect(missing_docs)] // temp

mod tests;

use anyhow::Context;
use guestmem::MemoryRead;
use vmbus_async::queue::IncomingPacket;
use vmbus_async::queue::OutgoingPacket;
use vmbus_async::queue::Queue;
use vmbus_channel::RawAsyncChannel;
use vmbus_ring::RingMem;
use vpci_protocol::MessageType;
use vpci_protocol::ProtocolVersion;
use vpci_protocol::QueryBusRelations2;
use vpci_protocol::QueryProtocolVersion;
use vpci_protocol::QueryProtocolVersionReply;
use vpci_protocol::Status;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

pub struct VpciClient<M: RingMem> {
    conn: VpciConnection<M>,
    protocol_version: ProtocolVersion,
}

struct VpciConnection<M: RingMem> {
    queue: Queue<M>,
    buf: Vec<u8>,
}

impl<M: RingMem> VpciConnection<M> {
    async fn send<S: IntoBytes + Immutable>(&mut self, send: S) -> anyhow::Result<()> {
        let mut write = self.queue.split().1;
        write
            .write(OutgoingPacket {
                transaction_id: 0,
                packet_type: vmbus_ring::OutgoingPacketType::InBandNoCompletion,
                payload: &[send.as_bytes()],
            })
            .await
            .context("failed to send protocol version query")?;
        Ok(())
    }

    async fn transact<
        S: IntoBytes + Immutable,
        R: FromBytes + IntoBytes + Immutable + KnownLayout,
    >(
        &mut self,
        send: S,
    ) -> anyhow::Result<R> {
        let (mut read, mut write) = self.queue.split();
        write
            .write(OutgoingPacket {
                transaction_id: 0,
                packet_type: vmbus_ring::OutgoingPacketType::InBandWithCompletion,
                payload: &[send.as_bytes()],
            })
            .await
            .context("failed to send protocol version query")?;

        let reply = read
            .read()
            .await
            .context("failed to read protocol version reply")?;
        let IncomingPacket::Completion(p) = &*reply else {
            anyhow::bail!("unexpected packet type")
        };
        let reply = p.reader().read_plain()?;
        Ok(reply)
    }
}

async fn negotiate<M: RingMem>(conn: &mut VpciConnection<M>) -> anyhow::Result<ProtocolVersion> {
    // Try to negotiate versions in order from newest to oldest
    let versions = &[ProtocolVersion::VB];

    for &version in versions {
        tracing::debug!(?version, "trying protocol version");

        // Create the protocol version query message
        let query = QueryProtocolVersion {
            message_type: MessageType::QUERY_PROTOCOL_VERSION,
            protocol_version: version,
        };

        let reply = conn
            .transact::<_, QueryProtocolVersionReply>(query)
            .await
            .context("failed to send protocol version query")?;
        if reply.status == Status::SUCCESS {
            return Ok(version);
        }
    }

    anyhow::bail!("no supported VPCI protocol version found");
}

impl<M: RingMem> VpciClient<M> {
    pub async fn connect(channel: RawAsyncChannel<M>) -> anyhow::Result<Self> {
        let mut conn = VpciConnection {
            queue: Queue::new(channel)?,
            buf: vec![0; vpci_protocol::MAXIMUM_PACKET_SIZE],
        };

        let version = negotiate(&mut conn)
            .await
            .context("failed to negotiate protocol version")?;

        let mut this = Self {
            conn,
            protocol_version: version,
        };

        this.conn
            .send(MessageType::QUERY_BUS_RELATIONS)
            .await
            .context("failed to send bus relations query")?;

        Ok(this)
    }

    async fn run(&mut self) {
        loop {
            let read = self.conn.queue.split().0;
            let packet = read
                .read()
                .await
                .context("failed to read protocol version reply")
                .unwrap();
            let IncomingPacket::Data(p) = &*packet else {
                tracing::warn!("unexpected packet type");
                continue;
            };

            let reply = p.reader().read_plain().unwrap();
            if reply.status == Status::SUCCESS {
                tracing::info!("successfully negotiated protocol version");
            } else {
                tracing::warn!("failed to negotiate protocol version");
            }
        }
    }
}
