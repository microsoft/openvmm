#![expect(missing_docs)] // temp

mod tests;

use anyhow::Context;
use guestmem::MemoryRead;
use inspect::Inspect;
use inspect::InspectMut;
use vmbus_async::queue::IncomingPacket;
use vmbus_async::queue::OutgoingPacket;
use vmbus_async::queue::Queue;
use vmbus_channel::RawAsyncChannel;
use vmbus_ring::RingMem;
use vpci_protocol as protocol;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

#[derive(InspectMut)]
pub struct VpciClient<M: RingMem> {
    conn: VpciConnection<M>,
    #[inspect(debug)]
    protocol_version: protocol::ProtocolVersion,
}

#[derive(Inspect)]
struct VpciConnection<M: RingMem> {
    queue: Queue<M>,
    #[inspect(skip)]
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

async fn negotiate<M: RingMem>(
    conn: &mut VpciConnection<M>,
) -> anyhow::Result<protocol::ProtocolVersion> {
    // Try to negotiate versions in order from newest to oldest
    let versions = &[protocol::ProtocolVersion::VB];

    for &version in versions {
        tracing::debug!(?version, "trying protocol version");

        // Create the protocol version query message
        let query = protocol::QueryProtocolVersion {
            message_type: protocol::MessageType::QUERY_PROTOCOL_VERSION,
            protocol_version: version,
        };

        let reply = conn
            .transact::<_, protocol::QueryProtocolVersionReply>(query)
            .await
            .context("failed to send protocol version query")?;
        if reply.status == protocol::Status::SUCCESS {
            return Ok(version);
        }
    }

    anyhow::bail!("no supported VPCI protocol version found");
}

pub trait MemoryAccess {
    fn gpa(&mut self) -> u64;
    fn read(&mut self, offset: u64) -> u32;
    fn write(&mut self, offset: u64, value: u32);
}

pub struct VpciDevice {}

impl Inspect for VpciDevice {
    fn inspect(&self, req: inspect::Request<'_>) {
        todo!()
    }
}

impl VpciDevice {
    pub fn read_cfg(&self, offset: u16) -> anyhow::Result<u32> {
        todo!()
    }

    pub fn write_cfg(&self, offset: u16, value: u32) -> anyhow::Result<()> {
        todo!()
    }

    pub async fn create_interrupt(&self, params: InterruptParams) -> anyhow::Result<()> {
        todo!()
    }

    pub async fn destroy_interrupt(&self, address: u64, data: u32) -> anyhow::Result<()> {
        todo!()
    }
}

pub struct InterruptParams {
    /// 32-bit interrupt vector number
    pub vector: u32,
    /// Interrupt delivery mode
    pub delivery_mode: u8,
    /// Number of interrupt vectors requested
    pub vector_count: u16,
    /// Array of processor IDs for interrupt affinity
    pub processor_array: Vec<u16>,
}

impl<M: RingMem> VpciClient<M> {
    pub async fn connect(
        channel: RawAsyncChannel<M>,
        mmio: Box<dyn MemoryAccess>,
    ) -> anyhow::Result<Self> {
        let mut conn = VpciConnection {
            queue: Queue::new(channel)?,
            buf: vec![0; protocol::MAXIMUM_PACKET_SIZE],
        };

        let version = negotiate(&mut conn)
            .await
            .context("failed to negotiate protocol version")?;

        let mut this = Self {
            conn,
            protocol_version: version,
        };

        let status: protocol::Status = this
            .conn
            .transact(protocol::FdoD0Entry {
                message_type: protocol::MessageType::FDO_D0_ENTRY,
                padding: 0,
                mmio_start: mmio.gpa(),
            })
            .await
            .context("failed to send FDO D0 entry")?;

        if status != protocol::Status::SUCCESS {
            anyhow::bail!("failed to enter D0 state: {:#x?}", status);
        }

        Ok(this)
    }

    async fn run(&mut self) -> anyhow::Result<()> {
        loop {
            let mut read = self.conn.queue.split().0;
            let packet = read
                .read()
                .await
                .context("failed to read protocol version reply")
                .unwrap();

            match &*packet {
                IncomingPacket::Data(p) => {
                    let mut reader = p.reader();
                    let len = reader.len();
                    let buf = self.conn.buf.get_mut(..len).context("packet too large")?;
                    reader.read(buf)?;
                    let (packet_type, _) = protocol::MessageType::read_from_prefix(buf)
                        .ok()
                        .context("packet too small")?;
                    match packet_type {
                        protocol::MessageType::BUS_RELATIONS2 => {
                            let (bus_relations, _) =
                                protocol::QueryBusRelations2::read_from_prefix(buf)
                                    .ok()
                                    .context("failed to read bus relations")?;
                            if bus_relations.device_count != 1 {
                                anyhow::bail!("only a single device is supported");
                            }
                            tracing::info!(?bus_relations, "bus relations");
                        }
                        p => {
                            anyhow::bail!("unexpected packet type: {:?}", p);
                        }
                    }
                }
                IncomingPacket::Completion(p) => {
                    todo!()
                }
            }
        }
    }
}
