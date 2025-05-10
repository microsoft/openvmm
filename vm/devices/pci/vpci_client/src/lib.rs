#![expect(missing_docs)] // temp

mod tests;

use anyhow::Context;
use futures::FutureExt;
use futures::StreamExt;
use futures_concurrency::future::Race;
use guestmem::MemoryRead;
use inspect::Inspect;
use inspect::InspectMut;
use mesh::rpc::FailableRpc;
use mesh::rpc::RpcSend;
use pal_async::task::Spawn;
use pal_async::task::Task;
use parking_lot::Mutex;
use pci_core::spec::hwid::HardwareIds;
use std::sync::Arc;
use vmbus_async::queue::IncomingPacket;
use vmbus_async::queue::OutgoingPacket;
use vmbus_async::queue::Queue;
use vmbus_channel::RawAsyncChannel;
use vmbus_ring::RingMem;
use vmcore::vpci_msi::MapVpciInterrupt;
use vmcore::vpci_msi::MsiAddressData;
use vmcore::vpci_msi::RegisterInterruptError;
use vpci_protocol as protocol;
use vpci_protocol::SlotNumber;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::Unalign;

pub struct VpciClient {
    req: mesh::Sender<WorkerRequest>,
    task: Task<()>,
}

impl Inspect for VpciClient {
    fn inspect(&self, req: inspect::Request<'_>) {
        self.req.send(WorkerRequest::Inspect(req.defer()))
    }
}

enum WorkerRequest {
    Inspect(inspect::Deferred),
    MapInterrupt(FailableRpc<protocol::CreateInterrupt2, protocol::MsiResourceRemapped>),
    UnmapInterrupt(FailableRpc<protocol::DeleteInterrupt, ()>),
}

#[derive(Inspect)]
struct VpciConnection<M: RingMem> {
    queue: Queue<M>,
    #[inspect(skip)]
    buf: Vec<u8>,
}

impl<M: RingMem> VpciConnection<M> {
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

pub trait MemoryAccess: Send {
    fn gpa(&mut self) -> u64;
    fn read(&mut self, addr: u64) -> u32;
    fn write(&mut self, addr: u64, value: u32);
}

#[derive(Inspect)]
pub struct VpciDevice {
    hw_ids: HardwareIds,
    #[inspect(skip)]
    config_space: Arc<Mutex<ConfigSpaceAccessor>>,
    #[inspect(with = "|&x| inspect::AsHex(u32::from(x))")]
    slot: SlotNumber,
    #[inspect(skip)]
    req: mesh::Sender<WorkerRequest>,
}

#[derive(Inspect)]
struct ConfigSpaceAccessor {
    #[inspect(skip)]
    mem: Box<dyn MemoryAccess>,
    base_gpa: u64,
    #[inspect(with = "|&x| inspect::AsHex(u32::from(x))")]
    current_slot: SlotNumber,
}

impl ConfigSpaceAccessor {
    fn set_slot(&mut self, slot: SlotNumber) {
        if slot != self.current_slot {
            self.mem
                .write(self.base_gpa + protocol::MMIO_PAGE_SLOT_NUMBER, slot.into());
            self.current_slot = slot;
        }
    }

    fn read(&mut self, slot: SlotNumber, offset: u16) -> u32 {
        self.set_slot(slot);
        self.mem
            .read(self.base_gpa + protocol::MMIO_PAGE_CONFIG_SPACE + offset as u64)
    }

    fn write(&mut self, slot: SlotNumber, offset: u16, value: u32) {
        self.set_slot(slot);
        self.mem.write(
            self.base_gpa + protocol::MMIO_PAGE_CONFIG_SPACE + offset as u64,
            value,
        );
    }
}

impl VpciDevice {
    pub fn hw_ids(&self) -> &HardwareIds {
        &self.hw_ids
    }

    pub fn read_cfg(&self, offset: u16) -> u32 {
        // TODO: for hardware IDs, use the cached values, both for efficiency
        // and so that the host cannot change them.
        self.config_space.lock().read(self.slot, offset)
    }

    pub fn write_cfg(&self, offset: u16, value: u32) {
        self.config_space.lock().write(self.slot, offset, value);
    }
}

impl MapVpciInterrupt for VpciDevice {
    async fn register_interrupt(
        &self,
        vector_count: u32,
        params: &vmcore::vpci_msi::VpciInterruptParameters<'_>,
    ) -> Result<MsiAddressData, RegisterInterruptError> {
        let mut interrupt = protocol::MsiResourceDescriptor2 {
            vector: params
                .vector
                .try_into()
                .expect("need to support resource 3 for ARM64"),
            delivery_mode: 0, // TODO
            vector_count: vector_count.try_into().expect("BUGBUG: fail to caller"),
            processor_count: 0,
            processor_array: [0; 32],
            reserved: 0,
        };
        for (d, &s) in interrupt
            .processor_array
            .iter_mut()
            .zip(params.target_processors)
        {
            *d = s.try_into().expect("BUGBUG: fail to caller");
            interrupt.processor_count += 1;
        }
        let resource = self
            .req
            .call_failable(
                WorkerRequest::MapInterrupt,
                protocol::CreateInterrupt2 {
                    message_type: protocol::MessageType::CREATE_INTERRUPT2,
                    slot: self.slot,
                    interrupt,
                },
            )
            .await
            .map_err(|err| RegisterInterruptError::new(err))?;

        tracing::debug!(
            address = resource.address,
            data = resource.data_payload,
            "registered interrupt"
        );

        Ok(MsiAddressData {
            address: resource.address,
            data: resource.data_payload,
        })
    }

    async fn unregister_interrupt(&self, address: u64, data: u32) {
        tracing::debug!(address, data, "unregistering interrupt");
        let resource = protocol::DeleteInterrupt {
            message_type: protocol::MessageType::DELETE_INTERRUPT,
            slot: self.slot,
            interrupt: protocol::MsiResourceRemapped {
                reserved: 0,
                message_count: 0, // The host does not look at this value, so don't bother to remember it.
                data_payload: data,
                address,
            },
        };
        self.req
            .call_failable(WorkerRequest::UnmapInterrupt, resource)
            .await
            .unwrap_or_else(|err| {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "failed to unregister interrupt"
                );
            });
    }
}

#[derive(InspectMut)]
struct VpciClientWorker<M: RingMem> {
    conn: VpciConnection<M>,
    #[inspect(iter_by_key)]
    tx: slab::Slab<Tx>,
    #[inspect(skip)]
    req: mesh::Receiver<WorkerRequest>,
    config_space: Arc<Mutex<ConfigSpaceAccessor>>,
    #[inspect(debug)]
    protocol_version: protocol::ProtocolVersion,
    #[inspect(skip)]
    send_devices: mesh::Sender<VpciDevice>,
}

#[derive(Inspect)]
#[inspect(external_tag)]
enum Tx {
    CreateInterrupt(#[inspect(skip)] FailableRpc<(), protocol::MsiResourceRemapped>),
    DeleteInterrupt(#[inspect(skip)] FailableRpc<(), ()>),
}

impl VpciClient {
    pub async fn connect<M: 'static + RingMem + Sync>(
        driver: impl Spawn,
        channel: RawAsyncChannel<M>,
        mut mmio: Box<dyn MemoryAccess>,
        devices: mesh::Sender<VpciDevice>,
    ) -> anyhow::Result<Self> {
        let mut conn = VpciConnection {
            queue: Queue::new(channel)?,
            buf: vec![0; protocol::MAXIMUM_PACKET_SIZE],
        };

        let version = negotiate(&mut conn)
            .await
            .context("failed to negotiate protocol version")?;

        let gpa = mmio.gpa();

        let status: protocol::Status = conn
            .transact(protocol::FdoD0Entry {
                message_type: protocol::MessageType::FDO_D0_ENTRY,
                padding: 0,
                mmio_start: gpa,
            })
            .await
            .context("failed to send FDO D0 entry")?;

        if status != protocol::Status::SUCCESS {
            anyhow::bail!("failed to enter D0 state: {:#x?}", status);
        }

        let (send, recv) = mesh::channel();
        let mut worker = VpciClientWorker {
            conn,
            tx: slab::Slab::new(),
            protocol_version: version,
            send_devices: devices,
            req: recv,
            config_space: Arc::new(Mutex::new(ConfigSpaceAccessor {
                mem: mmio,
                base_gpa: gpa,
                current_slot: (!0).into(),
            })),
        };

        let task = driver.spawn("vpci-client", async move {
            if let Err(err) = worker.run().await {
                tracing::error!(
                    error = err.as_ref() as &dyn std::error::Error,
                    "vpci client worker failed"
                );
            }
        });

        Ok(Self { task, req: send })
    }

    pub async fn shutdown(self) {
        drop(self.req);
        self.task.await;
    }

    pub fn detach(self) {
        self.task.detach();
    }
}

impl<M: RingMem> VpciClientWorker<M> {
    async fn run(&mut self) -> anyhow::Result<()> {
        loop {
            let req = {
                enum Event<T, U> {
                    Packet(T),
                    Request(U),
                }

                let mut read = self.conn.queue.split().0;
                let read_packet = read.read().map(Event::Packet);
                let req = self.req.next().map(Event::Request);

                let event = (read_packet, req).race().await;
                match event {
                    Event::Packet(p) => {
                        let p = p.context("failed to read packet")?;
                        match &*p {
                            IncomingPacket::Data(p) => {
                                let mut reader = p.reader();
                                let len = reader.len();
                                let buf =
                                    self.conn.buf.get_mut(..len).context("packet too large")?;
                                reader.read(buf)?;
                                let (packet_type, _) = protocol::MessageType::read_from_prefix(buf)
                                    .ok()
                                    .context("packet too small")?;
                                match packet_type {
                                    protocol::MessageType::BUS_RELATIONS2 => {
                                        let (bus_relations, devices) =
                                            protocol::QueryBusRelations2::read_from_prefix(buf)
                                                .ok()
                                                .context("failed to read bus relations")?;

                                        let (devices, _) = <[Unalign<
                                            protocol::DeviceDescription2,
                                        >]>::ref_from_prefix_with_elems(
                                            devices,
                                            bus_relations.device_count as usize,
                                        )
                                        .ok()
                                        .context("failed to read bus relation devices")?;
                                        for device in devices {
                                            let device = device.get();
                                            let hw_ids = HardwareIds {
                                                vendor_id: device.pnp_id.vendor_id,
                                                device_id: device.pnp_id.device_id,
                                                revision_id: device.pnp_id.revision_id,
                                                prog_if: device.pnp_id.prog_if.into(),
                                                sub_class: device.pnp_id.sub_class.into(),
                                                base_class: device.pnp_id.base_class.into(),
                                                type0_sub_vendor_id: device
                                                    .pnp_id
                                                    .sub_vendor_id
                                                    .into(),
                                                type0_sub_system_id: device
                                                    .pnp_id
                                                    .sub_system_id
                                                    .into(),
                                            };
                                            let vpci_device = VpciDevice {
                                                hw_ids,
                                                config_space: self.config_space.clone(),
                                                slot: device.slot,
                                                req: self.req.sender(),
                                            };
                                            self.send_devices.send(vpci_device);
                                        }
                                    }
                                    p => {
                                        anyhow::bail!("unexpected packet type: {:?}", p);
                                    }
                                }
                            }
                            IncomingPacket::Completion(p) => {
                                let tx = p.transaction_id() as usize;
                                let entry =
                                    self.tx.try_remove(tx).context("failed to find tx entry")?;
                                match entry {
                                    Tx::CreateInterrupt(rpc) => {
                                        let reply = p
                                            .reader()
                                            .read_plain::<protocol::CreateInterruptReply>()
                                            .context("failed to read create interrupt reply")?;
                                        if reply.status == protocol::Status::SUCCESS {
                                            rpc.complete(Ok(reply.interrupt));
                                        } else {
                                            rpc.fail(anyhow::anyhow!(
                                                "failed to create interrupt: {:#x?}",
                                                reply.status
                                            ));
                                        }
                                    }
                                    Tx::DeleteInterrupt(rpc) => {
                                        let status = p
                                            .reader()
                                            .read_plain::<protocol::Status>()
                                            .context("failed to read delete interrupt reply")?;
                                        if status == protocol::Status::SUCCESS {
                                            rpc.complete(Ok(()));
                                        } else {
                                            rpc.fail(anyhow::anyhow!(
                                                "failed to delete interrupt: {:#x?}",
                                                status
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                        None
                    }
                    Event::Request(Some(req)) => Some(req),
                    Event::Request(None) => break,
                }
            };
            if let Some(req) = req {
                self.handle_req(req).await?;
            }
        }
        todo!("cleanly tear down");
    }

    async fn handle_req(&mut self, req: WorkerRequest) -> anyhow::Result<()> {
        match req {
            WorkerRequest::Inspect(deferred) => deferred.inspect(&mut *self),
            WorkerRequest::MapInterrupt(rpc) => {
                let (req, reply) = rpc.split();
                let entry = self.tx.vacant_entry();
                self.conn
                    .queue
                    .split()
                    .1
                    .write(OutgoingPacket {
                        transaction_id: entry.key() as u64,
                        packet_type: vmbus_ring::OutgoingPacketType::InBandWithCompletion,
                        payload: &[req.as_bytes()],
                    })
                    .await
                    .context("failed to send create interrupt message")?;

                entry.insert(Tx::CreateInterrupt(reply));
            }
            WorkerRequest::UnmapInterrupt(rpc) => {
                let (req, reply) = rpc.split();
                let entry = self.tx.vacant_entry();
                self.conn
                    .queue
                    .split()
                    .1
                    .write(OutgoingPacket {
                        transaction_id: entry.key() as u64,
                        packet_type: vmbus_ring::OutgoingPacketType::InBandWithCompletion,
                        payload: &[req.as_bytes()],
                    })
                    .await
                    .context("failed to send delete interrupt message")?;

                entry.insert(Tx::DeleteInterrupt(reply));
            }
        }
        Ok(())
    }
}
