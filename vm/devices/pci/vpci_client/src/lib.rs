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
use pci_core::spec::cfg_space::Command;
use pci_core::spec::cfg_space::HeaderType00;
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
use zerocopy::FromZeros;
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
    QueryResourceRequirements(FailableRpc<SlotNumber, protocol::QueryResourceRequirementsReply>),
    Init(FailableRpc<SlotNumber, ()>),
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
                transaction_id: 1,
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
            tracing::debug!(?version, "negotiated protocol version");
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
pub struct VpciDeviceDescription {
    hw_ids: HardwareIds,
    #[inspect(skip)]
    config_space: Arc<Mutex<ConfigSpaceAccessor>>,
    #[inspect(hex, with = "|&x| u32::from(x)")]
    slot: SlotNumber,
    #[inspect(skip)]
    req: mesh::Sender<WorkerRequest>,
}

#[derive(Inspect)]
pub struct VpciDevice {
    #[inspect(flatten)]
    desc: VpciDeviceDescription,
    shadows: Mutex<ConfigSpaceShadows>,
    #[inspect(hex, iter_by_index)]
    bar_masks: [u32; 6],
    #[inspect(hex, iter_by_index)]
    bar_rao: [u32; 6],
}

#[derive(Inspect)]
struct ConfigSpaceAccessor {
    #[inspect(skip)]
    mem: Box<dyn MemoryAccess>,
    base_gpa: u64,
    #[inspect(hex, with = "|&x| u32::from(x)")]
    current_slot: SlotNumber,
}

#[derive(Inspect)]
struct ConfigSpaceShadows {
    command: Command,
    #[inspect(hex, iter_by_index)]
    bars: [u32; 6],
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
        let value = self
            .mem
            .read(self.base_gpa + protocol::MMIO_PAGE_CONFIG_SPACE + offset as u64);
        tracing::trace!(?slot, offset, value, "host config space read");
        value
    }

    fn write(&mut self, slot: SlotNumber, offset: u16, value: u32) {
        self.set_slot(slot);
        tracing::trace!(?slot, offset, value, "host config space write");
        self.mem.write(
            self.base_gpa + protocol::MMIO_PAGE_CONFIG_SPACE + offset as u64,
            value,
        );
    }
}

impl VpciDeviceDescription {
    pub fn hw_ids(&self) -> &HardwareIds {
        &self.hw_ids
    }

    pub async fn init(self) -> anyhow::Result<VpciDevice> {
        let requirements = self
            .req
            .call_failable(WorkerRequest::QueryResourceRequirements, self.slot)
            .await?;

        tracing::info!(
            bars = format_args!("{:#x?}", requirements.bars),
            "queried requirements"
        );

        self.req
            .call_failable(WorkerRequest::Init, self.slot)
            .await?;

        let mut high64 = false;
        let mut bar_rao = [0; 6];
        for ((i, &bar), rao) in requirements.bars.iter().enumerate().zip(&mut bar_rao) {
            if high64 {
                high64 = false;
                *rao = 0;
            } else {
                let bits = pci_core::spec::cfg_space::BarEncodingBits::from(bar);
                if bits.use_pio() {
                    anyhow::bail!("BAR {} is PIO, which is not supported by VPCI", i);
                }
                *rao = bar & 0xf;
                high64 = bits.type_64_bit();
            }
        }

        let device = VpciDevice {
            desc: self,
            shadows: Mutex::new(ConfigSpaceShadows {
                command: Command::new(),
                bars: [0; 6],
            }),
            bar_masks: requirements.bars,
            bar_rao,
        };

        Ok(device)
    }
}

impl VpciDevice {
    pub fn read_cfg(&self, offset: u16) -> u32 {
        // For static values, return values from the device's description.
        let value = match HeaderType00(offset) {
            HeaderType00::STATUS_COMMAND => {
                let shadows = self.shadows.lock();
                let status_command = self.desc.config_space.lock().read(self.desc.slot, offset);
                // Preserve the MMIO enabled bit in the command register, since
                // Hyper-V does not always emulate it correctly for reads.
                let mask = u32::from(u16::from(Command::new().with_mmio_enabled(true)));
                (status_command & !mask) | (u32::from(u16::from(shadows.command)) & mask)
            }
            HeaderType00::DEVICE_VENDOR => {
                (self.desc.hw_ids.vendor_id as u32) | ((self.desc.hw_ids.device_id as u32) << 16)
            }
            HeaderType00::CLASS_REVISION => {
                (self.desc.hw_ids.revision_id as u32)
                    | ((self.desc.hw_ids.prog_if.0 as u32) << 8)
                    | ((self.desc.hw_ids.sub_class.0 as u32) << 16)
                    | ((self.desc.hw_ids.base_class.0 as u32) << 24)
            }
            HeaderType00::SUBSYSTEM_ID => {
                (self.desc.hw_ids.type0_sub_vendor_id as u32)
                    | ((self.desc.hw_ids.type0_sub_system_id as u32) << 16)
            }
            HeaderType00::BAR0
            | HeaderType00::BAR1
            | HeaderType00::BAR2
            | HeaderType00::BAR3
            | HeaderType00::BAR4
            | HeaderType00::BAR5 => {
                // The Hyper-V VPCI implementation does not consistently handle
                // BAR reads. Return the shadowed value.
                let shadows = self.shadows.lock();
                let i = (offset - HeaderType00::BAR0.0) as usize / 4;
                shadows.bars[i] | self.bar_rao[i]
            }
            _ => self.desc.config_space.lock().read(self.desc.slot, offset),
        };
        tracing::trace!(?offset, value, "config space read");
        value
    }

    pub fn write_cfg(&self, offset: u16, value: u32) {
        tracing::trace!(?offset, value, "config space write");
        let mut shadows = self.shadows.lock();
        let shadows = &mut *shadows;
        let mut accessor = self.desc.config_space.lock();
        match HeaderType00(offset) {
            HeaderType00::STATUS_COMMAND => {
                let new_command = Command::from(value as u16);
                if new_command.mmio_enabled() && !shadows.command.mmio_enabled() {
                    // Flush the BAR shadow to the device.
                    for (i, &bar) in shadows.bars.iter().enumerate() {
                        let bar_offset = HeaderType00::BAR0.0 + (i as u16 * 4);
                        accessor.write(self.desc.slot, bar_offset, bar);
                    }
                }
                shadows.command = new_command;
            }
            HeaderType00::BAR0
            | HeaderType00::BAR1
            | HeaderType00::BAR2
            | HeaderType00::BAR3
            | HeaderType00::BAR4
            | HeaderType00::BAR5 => {
                // Write the BAR shadow. Defer writing to the device until MMIO
                // is enabled to avoid wasting time writing probe values to the
                // host.
                let i = (offset - HeaderType00::BAR0.0) as usize / 4;
                shadows.bars[i] = value & self.bar_masks[i] | self.bar_rao[i];
                return;
            }
            _ => {}
        }
        accessor.write(self.desc.slot, offset, value);
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
            .desc
            .req
            .call_failable(
                WorkerRequest::MapInterrupt,
                protocol::CreateInterrupt2 {
                    message_type: protocol::MessageType::CREATE_INTERRUPT2,
                    slot: self.desc.slot,
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
            slot: self.desc.slot,
            interrupt: protocol::MsiResourceRemapped {
                reserved: 0,
                message_count: 0, // The host does not look at this value, so don't bother to remember it.
                data_payload: data,
                address,
            },
        };
        self.desc
            .req
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
    send_devices: mesh::Sender<VpciDeviceDescription>,
}

#[derive(Inspect)]
#[inspect(external_tag)]
enum Tx {
    FdoD0Entry(#[inspect(skip)] mesh::OneshotSender<protocol::Status>),
    CreateInterrupt(#[inspect(skip)] FailableRpc<(), protocol::MsiResourceRemapped>),
    DeleteInterrupt(#[inspect(skip)] FailableRpc<(), ()>),
    QueryResourceRequirements(
        #[inspect(skip)] FailableRpc<(), protocol::QueryResourceRequirementsReply>,
    ),
    AssignedResources(#[inspect(skip)] FailableRpc<(), ()>),
}

impl VpciClient {
    pub async fn connect<M: 'static + RingMem + Sync>(
        driver: impl Spawn,
        channel: RawAsyncChannel<M>,
        mut mmio: Box<dyn MemoryAccess>,
        devices: mesh::Sender<VpciDeviceDescription>,
    ) -> anyhow::Result<Self> {
        let mut conn = VpciConnection {
            queue: Queue::new(channel)?,
            buf: vec![0; protocol::MAXIMUM_PACKET_SIZE],
        };

        let version = negotiate(&mut conn)
            .await
            .context("failed to negotiate protocol version")?;

        let gpa = mmio.gpa();

        tracing::debug!(gpa, "requesting fdo d0 entry");

        let mut tx = slab::Slab::new();

        // Start a transaction to move the bus to the D0 state. The completion
        // may come after the device list, so start the task and wait for the
        // reply afterwards.
        let (fdo_entry_send, fdo_entry_recv) = mesh::oneshot();
        let tx_id = index_to_tx_id(tx.insert(Tx::FdoD0Entry(fdo_entry_send)));
        conn.queue
            .split()
            .1
            .write(OutgoingPacket {
                transaction_id: tx_id,
                packet_type: vmbus_ring::OutgoingPacketType::InBandWithCompletion,
                payload: &[protocol::FdoD0Entry {
                    message_type: protocol::MessageType::FDO_D0_ENTRY,
                    padding: 0,
                    mmio_start: gpa,
                }
                .as_bytes()],
            })
            .await
            .context("failed to send FDO D0 entry")?;

        let (send, recv) = mesh::channel();
        let mut worker = VpciClientWorker {
            conn,
            tx,
            protocol_version: version,
            send_devices: devices,
            req: recv,
            config_space: Arc::new(Mutex::new(ConfigSpaceAccessor {
                mem: mmio,
                base_gpa: gpa,
                // Let's not assume the config space access starts at slot 0.
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

        let status = fdo_entry_recv
            .await
            .context("no response to FDO D0 entry")?;

        if status != protocol::Status::SUCCESS {
            task.cancel().await;
            anyhow::bail!("failed to enter D0 state: {:#x?}", status);
        }

        tracing::debug!(gpa, "fdo d0 entry successful");

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

                                tracing::debug!(?packet_type, "received packet");

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
                                            let vpci_device = VpciDeviceDescription {
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
                                let tx_id = p.transaction_id();
                                let entry = self
                                    .tx
                                    .try_remove(tx_id_to_index(tx_id))
                                    .context("failed to find tx entry")?;

                                let status = p
                                    .reader()
                                    .read_plain::<protocol::Status>()
                                    .context("failed to read tx reply")?;

                                match entry {
                                    Tx::FdoD0Entry(send) => {
                                        tracing::trace!(
                                            tx_id,
                                            ?status,
                                            "fdo d0 entry reply received"
                                        );
                                        send.send(status);
                                    }
                                    Tx::CreateInterrupt(rpc) => {
                                        tracing::trace!(
                                            tx_id,
                                            ?status,
                                            "create interrupt reply received"
                                        );

                                        if status == protocol::Status::SUCCESS {
                                            let reply = p
                                                .reader()
                                                .read_plain::<protocol::CreateInterruptReply>()
                                                .context("failed to read create interrupt reply")?;
                                            rpc.complete(Ok(reply.interrupt));
                                        } else {
                                            rpc.fail(anyhow::anyhow!(
                                                "failed to create interrupt: {status:#x?}",
                                            ));
                                        }
                                    }
                                    Tx::DeleteInterrupt(rpc) => {
                                        tracing::trace!(tx_id, "delete interrupt reply received");

                                        if status == protocol::Status::SUCCESS {
                                            rpc.complete(Ok(()));
                                        } else {
                                            rpc.fail(anyhow::anyhow!(
                                                "failed to delete interrupt: {status:#x?}",
                                            ));
                                        }
                                    }
                                    Tx::AssignedResources(rpc) => {
                                        tracing::trace!(
                                            tx_id,
                                            ?status,
                                            "assigned resources reply received"
                                        );

                                        if status == protocol::Status::SUCCESS {
                                            rpc.complete(Ok(()));
                                        } else {
                                            rpc.fail(anyhow::anyhow!(
                                                "failed to initialize device: {status:#x?}",
                                            ));
                                        }
                                    }
                                    Tx::QueryResourceRequirements(rpc) => {
                                        tracing::trace!(
                                            tx_id,
                                            ?status,
                                            "query resource requirements reply received"
                                        );

                                        if status == protocol::Status::SUCCESS {
                                            let reply = p
                                                .reader()
                                                .read_plain::<protocol::QueryResourceRequirementsReply>()
                                                .context("failed to read query resource requirements reply")?;
                                            rpc.complete(Ok(reply));
                                        } else {
                                            rpc.fail(anyhow::anyhow!(
                                                "failed to query resource requirements: {status:#x?}",
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
                self.send_tx(Tx::CreateInterrupt(reply), req, &[])
                    .await
                    .context("failed to send create interrupt message")?;
            }
            WorkerRequest::UnmapInterrupt(rpc) => {
                let (req, reply) = rpc.split();
                self.send_tx(Tx::DeleteInterrupt(reply), req, &[])
                    .await
                    .context("failed to send delete interrupt message")?;
            }
            WorkerRequest::Init(rpc) => {
                let (slot, reply) = rpc.split();
                // Send space for one resource to satisfy the Hyper-V implementation.
                self.send_tx(
                    Tx::AssignedResources(reply),
                    protocol::DeviceTranslate {
                        message_type: protocol::MessageType::ASSIGNED_RESOURCES,
                        slot: slot.into(),
                        ..FromZeros::new_zeroed()
                    },
                    &[0; size_of::<vpci_protocol::MsiResource3>()],
                )
                .await
                .context("failed to send assigned resources request")?;
            }
            WorkerRequest::QueryResourceRequirements(rpc) => {
                let (slot, reply) = rpc.split();
                self.send_tx(
                    Tx::QueryResourceRequirements(reply),
                    protocol::QueryResourceRequirements {
                        message_type: protocol::MessageType::CURRENT_RESOURCE_REQUIREMENTS,
                        slot: slot.into(),
                    },
                    &[],
                )
                .await
                .context("failed to send query resource requirements request")?;
            }
        }
        Ok(())
    }

    async fn send_tx<S: IntoBytes + Immutable>(
        &mut self,
        tx: Tx,
        msg: S,
        extra: &[u8],
    ) -> anyhow::Result<()> {
        let entry = self.tx.vacant_entry();
        let tx_id = index_to_tx_id(entry.key());
        tracing::trace!(
            tx_id,
            message = std::any::type_name_of_val(&msg),
            "sending transaction"
        );

        self.conn
            .queue
            .split()
            .1
            .write(OutgoingPacket {
                transaction_id: tx_id,
                packet_type: vmbus_ring::OutgoingPacketType::InBandWithCompletion,
                payload: &[msg.as_bytes(), extra],
            })
            .await
            .context("failed to send transaction")?;

        entry.insert(tx);
        Ok(())
    }
}

fn index_to_tx_id(index: usize) -> u64 {
    // Hyper-V VPCI doesn't like transaction IDs of 0, so we start at 1.
    (index + 1) as u64
}

fn tx_id_to_index(tx_id: u64) -> usize {
    tx_id.saturating_sub(1) as usize
}
