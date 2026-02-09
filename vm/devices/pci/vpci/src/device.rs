// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtual PCI device module

use async_trait::async_trait;
use chipset_device::ChipsetDevice;
use chipset_device::mmio::ControlMmioIntercept;
use closeable_mutex::CloseableMutex;
use guestmem::AccessError;
use guestmem::MemoryRead;
use guid::Guid;
use inspect::Inspect;
use inspect::InspectMut;
use pci_core::bar_mapping::BarMappings;
use pci_core::chipset_device_ext::PciChipsetDeviceExt;
use pci_core::spec::cfg_space;
use pci_core::spec::hwid::HardwareIds;
use ring::OutgoingPacketType;
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use task_control::Cancelled;
use task_control::StopTask;
use thiserror::Error;
use tracing::Instrument;
use vmbus_async::queue;
use vmbus_async::queue::IncomingPacket;
use vmbus_async::queue::OutgoingPacket;
use vmbus_async::queue::Queue;
use vmbus_channel::RawAsyncChannel;
use vmbus_channel::bus::OfferParams;
use vmbus_channel::channel::ChannelOpenError;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_channel::simple::SaveRestoreSimpleVmbusDevice;
use vmbus_channel::simple::SimpleVmbusDevice;
use vmbus_ring as ring;
use vmbus_ring::RingMem;
use vmcore::save_restore::NoSavedState;
use vmcore::vpci_msi::MapVpciInterrupt;
use vmcore::vpci_msi::MsiAddressData;
use vmcore::vpci_msi::RegisterInterruptError;
use vmcore::vpci_msi::VpciInterruptMapper;
use vmcore::vpci_msi::VpciInterruptParameters;
use vpci_protocol as protocol;
use vpci_protocol::SlotNumber;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::Ref;

const PCI_MAX_MSI_VECTOR_COUNT: u16 = 32;

const VPCI_MESSAGE_RESOURCE_2_MAX_CPU_COUNT: u32 = 32;

#[derive(Debug, Copy, Clone, Default)]
struct MmioResource {
    address: u64,
    len: u64,
}

impl MmioResource {
    fn from_protocol(desc: &protocol::PartialResourceDescriptor) -> Result<Self, PacketError> {
        let shift = match desc.resource_type {
            protocol::ResourceType::MEMORY => 0,
            protocol::ResourceType::MEMORY_LARGE => {
                if desc.flags.large_40() {
                    8
                } else if desc.flags.large_48() {
                    16
                } else if desc.flags.large_64() {
                    32
                } else {
                    return Err(PacketError::InvalidMmio);
                }
            }
            _ => return Ok(Self { address: 0, len: 0 }),
        };
        Ok(Self {
            address: desc.address.into(),
            len: (desc.adjusted_len as u64) << shift,
        })
    }

    fn to_protocol(self) -> protocol::PartialResourceDescriptor {
        let len = self.len;
        let mut flags = protocol::ResourceFlags::new();
        let (resource_type, shift) = if len == 0 {
            return FromZeros::new_zeroed();
        } else if len < 1 << 32 {
            (protocol::ResourceType::MEMORY, 0)
        } else if len < 1 << 40 {
            flags.set_large_40(true);
            (protocol::ResourceType::MEMORY_LARGE, 8)
        } else if len < 1 << 48 {
            flags.set_large_48(true);
            (protocol::ResourceType::MEMORY_LARGE, 16)
        } else {
            flags.set_large_64(true);
            (protocol::ResourceType::MEMORY_LARGE, 32)
        };

        // Strip the low bits, rounding up if any were set.
        let adjusted_len = (((len - 1) >> shift) + 1) as u32;

        protocol::PartialResourceDescriptor {
            resource_type,
            share_disposition: 0,
            flags,
            address: self.address.into(),
            adjusted_len,
            padding: 0,
        }
    }
}

#[derive(Debug)]
struct ResourceRequests {
    mmio_ranges: [MmioResource; 6],
    interrupts: Vec<InterruptResourceRequest>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum InterruptType {
    Fixed,
    LowestPriority,
}

#[derive(Debug)]
struct InterruptResourceRequest {
    vector: u32,
    vector_count: u8,
    delivery_mode: InterruptType,
    target_processors: Vec<u32>,
}

impl InterruptResourceRequest {
    fn from_protocol(desc: &protocol::MsiResourceDescriptor) -> Result<Self, PacketError> {
        let vector_count = desc.vector_count;
        let processor_count = desc.processor_mask.count_ones();
        if vector_count > PCI_MAX_MSI_VECTOR_COUNT
            || processor_count > VPCI_MESSAGE_RESOURCE_2_MAX_CPU_COUNT
        {
            return Err(PacketError::InvalidInterrupt);
        }
        let target_processors = (0..64)
            .filter(|x| desc.processor_mask & (1 << x) != 0)
            .collect();

        Ok(Self {
            vector: desc.vector.into(),
            vector_count: vector_count as u8,
            delivery_mode: get_interrupt_type(desc.delivery_mode)?,
            target_processors,
        })
    }

    fn from_protocol2(desc: &protocol::MsiResourceDescriptor2) -> Result<Self, PacketError> {
        let vector_count = desc.vector_count;
        let processor_count = desc.processor_count;
        if vector_count > PCI_MAX_MSI_VECTOR_COUNT {
            return Err(PacketError::InvalidInterrupt);
        }
        let target_processors = desc
            .processor_array
            .get(..processor_count as usize)
            .ok_or(PacketError::InvalidInterrupt)?
            .iter()
            .map(|&v| v.into())
            .collect();

        Ok(Self {
            vector: desc.vector.into(),
            vector_count: vector_count as u8,
            delivery_mode: get_interrupt_type(desc.delivery_mode)?,
            target_processors,
        })
    }

    fn from_protocol3(desc: &protocol::MsiResourceDescriptor3) -> Result<Self, PacketError> {
        let vector_count = desc.vector_count;
        let processor_count = desc.processor_count;
        if vector_count > PCI_MAX_MSI_VECTOR_COUNT {
            return Err(PacketError::InvalidInterrupt);
        }
        let target_processors = desc
            .processor_array
            .get(..processor_count as usize)
            .ok_or(PacketError::InvalidInterrupt)?
            .iter()
            .map(|&v| v.into())
            .collect();

        Ok(Self {
            vector: desc.vector,
            vector_count: vector_count as u8,
            delivery_mode: get_interrupt_type(desc.delivery_mode)?,
            target_processors,
        })
    }
}

#[derive(Debug, Error)]
enum PacketError {
    #[error("unknown packet type {0:?}")]
    UnknownType(protocol::MessageType),
    #[error("memory access error")]
    Access(#[source] AccessError),
    #[error("invalid interrupt type {0:?}")]
    InvalidInterruptType(vpci_protocol::DeliveryMode),
    #[error("invalid interrupt resources")]
    InvalidInterrupt,
    #[error("packet is too small: {0}")]
    PacketTooSmall(&'static str),
    #[error("packet is too large")]
    PacketTooLarge,
    #[error("invalid mmio resource")]
    InvalidMmio,
    #[error("invalid bars")]
    InvalidBars(#[source] InvalidBars),
    #[error("invalid slot {0:?}")]
    InvalidSlot(SlotNumber),
    #[error("msi resource count {0} too high")]
    TooManyMsis(u32),
    #[error("failed to register interrupt")]
    RegisterInterrupt(#[source] RegisterInterruptError),
    #[error("unknown interrupt address {:#x}/data {:#x}", .0.address, .0.data)]
    UnknownInterrupt(MsiAddressData),
}

#[derive(Debug)]
enum PacketData {
    QueryProtocolVersion {
        version: protocol::ProtocolVersion,
    },
    FdoD0Entry {
        mmio_start: u64,
    },
    FdoD0Exit,
    QueryRelations,
    DeviceRequest {
        slot: SlotNumber,
        request: DeviceRequest,
    },
}

#[derive(Debug)]
enum DeviceRequest {
    AssignedResources {
        resources: ResourceRequests,
        reply_type: AssignedResourcesReplyType,
    },
    CreateInterrupt {
        interrupt: InterruptResourceRequest,
    },
    DeleteInterrupt {
        interrupt: protocol::MsiResourceRemapped,
    },
    QueryResources,
    GetResources,
    DevicePowerChange {
        target_state: protocol::DevicePowerState,
    },
    ReleaseResources,
    Reset,
}

#[derive(Debug)]
enum AssignedResourcesReplyType {
    V1,
    V2,
}

fn get_interrupt_type(mode: vpci_protocol::DeliveryMode) -> Result<InterruptType, PacketError> {
    match mode {
        vpci_protocol::DeliveryMode::FIXED => Ok(InterruptType::Fixed),
        vpci_protocol::DeliveryMode::LOWEST_PRIORITY => Ok(InterruptType::LowestPriority),
        _ => Err(PacketError::InvalidInterruptType(mode)),
    }
}

fn parse_packet<T: RingMem>(packet: &queue::DataPacket<'_, T>) -> Result<PacketData, PacketError> {
    let mut buf = vec![0u64; protocol::MAXIMUM_PACKET_SIZE / 8];
    let mut reader = packet.reader();
    let len = reader.len();
    let buf = buf
        .as_mut_bytes()
        .get_mut(..len)
        .ok_or(PacketError::PacketTooLarge)?;

    reader.read(buf).map_err(PacketError::Access)?;
    let buf = &*buf;
    let message_type = protocol::MessageType::read_from_prefix(buf)
        .map_err(|_| PacketError::PacketTooSmall("header"))?
        .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

    tracing::trace!(?message_type, "parsing vpci packet");

    let data = match message_type {
        protocol::MessageType::ASSIGNED_RESOURCES
        | protocol::MessageType::ASSIGNED_RESOURCES2
        | protocol::MessageType::ASSIGNED_RESOURCES3 => {
            let (msg, rest) = Ref::<_, protocol::DeviceTranslate>::from_prefix(buf)
                .map_err(|_| PacketError::PacketTooSmall("translate"))?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

            if msg.msi_resource_count > protocol::MAX_SUPPORTED_INTERRUPT_MESSAGES {
                return Err(PacketError::TooManyMsis(msg.msi_resource_count));
            }

            let mmio_ranges = msg
                .mmio_resources
                .iter()
                .map(MmioResource::from_protocol)
                .collect::<Result<Vec<_>, _>>()?;

            let (reply_type, interrupts) = match message_type {
                protocol::MessageType::ASSIGNED_RESOURCES => (
                    AssignedResourcesReplyType::V1,
                    <[protocol::MsiResource]>::ref_from_prefix_with_elems(
                        rest,
                        msg.msi_resource_count as usize,
                    )
                    .map_err(|_| PacketError::PacketTooSmall("msi"))? // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                    .0
                    .iter()
                    .map(|rsrc| InterruptResourceRequest::from_protocol(rsrc.descriptor()))
                    .collect::<Result<Vec<_>, _>>()?,
                ),
                protocol::MessageType::ASSIGNED_RESOURCES2 => (
                    AssignedResourcesReplyType::V2,
                    <[protocol::MsiResource2]>::ref_from_prefix_with_elems(
                        rest,
                        msg.msi_resource_count as usize,
                    )
                    .map_err(|_| PacketError::PacketTooSmall("msi2"))? // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                    .0
                    .iter()
                    .map(|rsrc| InterruptResourceRequest::from_protocol2(rsrc.descriptor()))
                    .collect::<Result<Vec<_>, _>>()?,
                ),
                protocol::MessageType::ASSIGNED_RESOURCES3 => (
                    // Weirdly enough, the reply still uses the V2 resource
                    // format even though the request has the V3 resource type.
                    // This is not lossy since the reply format is the same between
                    // V2 and V3; V3 just has more padding in order to fit the V3
                    // request format.
                    AssignedResourcesReplyType::V2,
                    <[protocol::MsiResource3]>::ref_from_prefix_with_elems(
                        rest,
                        msg.msi_resource_count as usize,
                    )
                    .map_err(|_| PacketError::PacketTooSmall("msi3"))?
                    .0
                    .iter()
                    .map(|rsrc| InterruptResourceRequest::from_protocol3(rsrc.descriptor()))
                    .collect::<Result<Vec<_>, _>>()?,
                ),
                _ => unreachable!(),
            };

            PacketData::DeviceRequest {
                slot: msg.slot,
                request: DeviceRequest::AssignedResources {
                    resources: ResourceRequests {
                        mmio_ranges: mmio_ranges.try_into().unwrap(),
                        interrupts,
                    },
                    reply_type,
                },
            }
        }
        protocol::MessageType::RELEASE_RESOURCES => {
            let msg = protocol::PdoMessage::read_from_prefix(buf)
                .map_err(|_| PacketError::PacketTooSmall("release"))?
                .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

            PacketData::DeviceRequest {
                slot: msg.slot,
                request: DeviceRequest::ReleaseResources,
            }
        }
        protocol::MessageType::CREATE_INTERRUPT => {
            let msg = protocol::CreateInterrupt::read_from_prefix(buf)
                .map_err(|_| PacketError::PacketTooSmall("interrupt"))?
                .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            PacketData::DeviceRequest {
                slot: msg.slot,
                request: DeviceRequest::CreateInterrupt {
                    interrupt: InterruptResourceRequest::from_protocol(&msg.interrupt)?,
                },
            }
        }
        protocol::MessageType::CREATE_INTERRUPT2 => {
            let msg = protocol::CreateInterrupt2::read_from_prefix(buf)
                .map_err(|_| PacketError::PacketTooSmall("interrupt2"))?
                .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            PacketData::DeviceRequest {
                slot: msg.slot,
                request: DeviceRequest::CreateInterrupt {
                    interrupt: InterruptResourceRequest::from_protocol2(&msg.interrupt)?,
                },
            }
        }
        protocol::MessageType::CREATE_INTERRUPT3 => {
            let msg = protocol::CreateInterrupt3::read_from_prefix(buf)
                .map_err(|_| PacketError::PacketTooSmall("interrupt3"))?
                .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            PacketData::DeviceRequest {
                slot: msg.slot,
                request: DeviceRequest::CreateInterrupt {
                    interrupt: InterruptResourceRequest::from_protocol3(&msg.interrupt)?,
                },
            }
        }
        protocol::MessageType::DELETE_INTERRUPT | protocol::MessageType::DELETE_INTERRUPT2 => {
            let msg = protocol::DeleteInterrupt::read_from_prefix(buf)
                .map_err(|_| PacketError::PacketTooSmall("delete_interrupt"))?
                .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            PacketData::DeviceRequest {
                slot: msg.slot,
                request: DeviceRequest::DeleteInterrupt {
                    interrupt: msg.interrupt,
                },
            }
        }
        protocol::MessageType::CURRENT_RESOURCE_REQUIREMENTS => {
            let msg = protocol::QueryResourceRequirements::read_from_prefix(buf)
                .map_err(|_| PacketError::PacketTooSmall("query_req"))?
                .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            PacketData::DeviceRequest {
                slot: msg.slot,
                request: DeviceRequest::QueryResources,
            }
        }
        protocol::MessageType::GET_RESOURCES => {
            let msg = protocol::GetResources::read_from_prefix(buf)
                .map_err(|_| PacketError::PacketTooSmall("get_resources"))?
                .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            PacketData::DeviceRequest {
                slot: msg.slot,
                request: DeviceRequest::GetResources,
            }
        }
        protocol::MessageType::FDO_D0_ENTRY => {
            let msg = protocol::FdoD0Entry::read_from_prefix(buf)
                .map_err(|_| PacketError::PacketTooSmall("power_on"))?
                .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            PacketData::FdoD0Entry {
                mmio_start: msg.mmio_start,
            }
        }
        protocol::MessageType::FDO_D0_EXIT => PacketData::FdoD0Exit,
        protocol::MessageType::QUERY_BUS_RELATIONS => PacketData::QueryRelations,
        protocol::MessageType::QUERY_PROTOCOL_VERSION => {
            let msg = protocol::QueryProtocolVersion::read_from_prefix(buf)
                .map_err(|_| PacketError::PacketTooSmall("query_version"))?
                .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            PacketData::QueryProtocolVersion {
                version: msg.protocol_version,
            }
        }
        protocol::MessageType::DEVICE_POWER_STATE_CHANGE => {
            let msg = protocol::DevicePowerChange::read_from_prefix(buf)
                .map_err(|_| PacketError::PacketTooSmall("device_power_state"))?
                .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            PacketData::DeviceRequest {
                slot: msg.slot,
                request: DeviceRequest::DevicePowerChange {
                    target_state: msg.target_state,
                },
            }
        }
        protocol::MessageType::RESET_DEVICE => {
            let msg = protocol::PdoMessage::read_from_prefix(buf)
                .map_err(|_| PacketError::PacketTooSmall("reset_device"))?
                .0;
            PacketData::DeviceRequest {
                slot: msg.slot,
                request: DeviceRequest::Reset,
            }
        }
        typ => return Err(PacketError::UnknownType(typ)),
    };
    Ok(data)
}

#[derive(Debug, Error)]
enum WorkerError {
    #[error("unexpected packet order")]
    UnexpectedPacketOrder,
    #[error("queue error")]
    Queue(#[source] queue::Error),
    #[error("unexpectedly out of ring space")]
    OutOfSpace,
    #[error("invalid packet type")]
    InvalidPacketType,
    #[error("packet handling error")]
    Packet(#[from] PacketError),
}

impl<T: RingMem> Connection<T> {
    async fn send_packet<
        P: IntoBytes + Debug + Immutable + KnownLayout + ?Sized,
        Q: IntoBytes + Debug + Immutable + KnownLayout + ?Sized,
    >(
        &mut self,
        payload: &P,
        more_payload: &Q,
    ) -> Result<(), WorkerError> {
        tracing::trace!(?payload, "send packet");
        self.queue
            .split()
            .1
            .write(OutgoingPacket {
                transaction_id: 0,
                packet_type: OutgoingPacketType::InBandNoCompletion,
                payload: &[payload.as_bytes(), more_payload.as_bytes()],
            })
            .await
            .map_err(WorkerError::Queue)
    }

    async fn wait_for_completion_space(&mut self) -> Result<(), WorkerError> {
        let (_, mut write) = self.queue.split();
        // Not all VSCs support the full maximum packet size.
        let len = ring::PacketSize::completion(protocol::MAXIMUM_PACKET_SIZE).min(write.capacity());
        write.wait_ready(len).await.map_err(WorkerError::Queue)
    }

    fn send_completion<P: IntoBytes + Debug + Immutable + KnownLayout>(
        &mut self,
        transaction_id: Option<u64>,
        payload: &P,
        extra: &[u8],
    ) -> Result<(), WorkerError> {
        if let Some(transaction_id) = transaction_id {
            tracing::trace!(?payload, "completion");
            self.queue
                .split()
                .1
                .try_write(&OutgoingPacket {
                    transaction_id,
                    packet_type: OutgoingPacketType::Completion,
                    payload: &[payload.as_bytes(), extra],
                })
                .map_err(|err| match err {
                    queue::TryWriteError::Full(_) => WorkerError::OutOfSpace,
                    queue::TryWriteError::Queue(err) => WorkerError::Queue(err),
                })?;
        }
        Ok(())
    }
}

/// The VPCI channel state.
pub struct VpciChannelState<T: RingMem = GpadlRingMem> {
    conn: Connection<T>,
    state: ProtocolState,
}

impl<T: RingMem> InspectMut for VpciChannelState<T> {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        let Self { conn, state } = &self;
        let mut resp = req.respond();
        let state = match state {
            ProtocolState::Init => "initializing",
            ProtocolState::Ready(state) => {
                resp.display("version", &format_args!("{:x?}", state.vpci_version));
                "ready"
            }
        };
        resp.field("state", state).merge(conn);
    }
}

#[derive(Inspect)]
struct Connection<T: RingMem> {
    queue: Queue<T>,
}

enum ProtocolState {
    Init,
    Ready(ReadyState),
}

struct ReadyState {
    send_device: bool,
    send_completion: Option<u64>,
    vpci_version: protocol::ProtocolVersion,
}

impl<T: RingMem> VpciChannelState<T> {
    async fn run(&mut self, dev: &mut VpciChannel) -> Result<(), WorkerError> {
        loop {
            match &mut self.state {
                ProtocolState::Ready(state) => break state.run(&mut self.conn, dev).await,
                ProtocolState::Init => {
                    self.conn.wait_for_completion_space().await?;

                    let (packet, transaction_id) = {
                        let mut queue = self.conn.queue.split().0;
                        let packet = queue.read().await.map_err(WorkerError::Queue)?;

                        let IncomingPacket::Data(data) = &*packet else {
                            return Err(WorkerError::InvalidPacketType);
                        };
                        let packet = parse_packet(data).map_err(WorkerError::Packet)?;
                        (packet, data.transaction_id())
                    };

                    if let PacketData::QueryProtocolVersion { version } = packet {
                        let status = match version {
                            protocol::ProtocolVersion::RS1
                            | protocol::ProtocolVersion::VB
                            | protocol::ProtocolVersion::FE
                            | protocol::ProtocolVersion::GE
                            | protocol::ProtocolVersion::DT => protocol::Status::SUCCESS,
                            _ => protocol::Status::REVISION_MISMATCH,
                        };

                        let reply = protocol::QueryProtocolVersionReply {
                            status,
                            protocol_version: protocol::ProtocolVersion::VB,
                        };

                        self.conn.send_completion(transaction_id, &reply, &[])?;

                        if status == protocol::Status::SUCCESS {
                            self.state = ProtocolState::Ready(ReadyState {
                                vpci_version: version,
                                send_device: false,
                                send_completion: None,
                            });
                        }
                    } else {
                        return Err(WorkerError::UnexpectedPacketOrder);
                    }
                }
            }
        }
    }
}

impl ReadyState {
    async fn send_child_device(
        &mut self,
        conn: &mut Connection<impl RingMem>,
        dev: &mut VpciChannel,
    ) -> Result<(), WorkerError> {
        // Enumerate the device within the guest
        let hardware_ids = &dev.hardware_ids;
        let pnp_id = protocol::PnpId {
            vendor_id: hardware_ids.vendor_id,
            device_id: hardware_ids.device_id,
            revision_id: hardware_ids.revision_id,
            prog_if: hardware_ids.prog_if.into(),
            sub_class: hardware_ids.sub_class.into(),
            base_class: hardware_ids.base_class.into(),
            sub_vendor_id: hardware_ids.type0_sub_vendor_id,
            sub_system_id: hardware_ids.type0_sub_system_id,
        };
        if self.vpci_version < protocol::ProtocolVersion::VB {
            let relations = protocol::QueryBusRelations {
                message_type: protocol::MessageType::BUS_RELATIONS,
                device_count: 1,
                device: [],
            };
            let device = protocol::DeviceDescription {
                pnp_id,
                slot: SlotNumber::new(),
                serial_num: dev.serial_num,
            };

            conn.send_packet(&relations, &device).await?;
        } else {
            let relations = protocol::QueryBusRelations2 {
                message_type: protocol::MessageType::BUS_RELATIONS2,
                device_count: 1,
                device: [],
            };
            let device = protocol::DeviceDescription2 {
                pnp_id,
                slot: SlotNumber::new(),
                serial_num: dev.serial_num,
                flags: 0,
                numa_node: 0,
                rsvd: 0,
            };

            conn.send_packet(&relations, &device).await?;
        }

        Ok(())
    }

    async fn run(
        &mut self,
        conn: &mut Connection<impl RingMem>,
        dev: &mut VpciChannel,
    ) -> Result<(), WorkerError> {
        loop {
            if self.send_device {
                let span =
                    tracing::trace_span!("vpci_send_child_device", instance_id = ?dev.instance_id);
                self.send_child_device(conn, dev).instrument(span).await?;
                self.send_device = false;
            }
            if let Some(transaction_id) = self.send_completion {
                conn.send_completion(Some(transaction_id), &protocol::Status::SUCCESS, &[])?;
                self.send_completion = None;
            }

            // Don't pull a packets off the ring until there is space for its completion.
            conn.wait_for_completion_space()
                .instrument(tracing::trace_span!("vpci_wait_for_completion_space", instance_id = ?dev.instance_id))
                .await?;

            let (packet, transaction_id) = {
                let (mut queue, _) = conn.queue.split();
                let packet = queue.read().await.map_err(WorkerError::Queue)?;
                let IncomingPacket::Data(data) = packet.as_ref() else {
                    return Err(WorkerError::InvalidPacketType);
                };
                (parse_packet(data), data.transaction_id())
            };

            let r = match packet {
                Ok(packet) => {
                    tracing::trace!(?packet, instance_id = ?dev.instance_id, "vpci packet");
                    let span = tracing::trace_span!("vpci_handle_packet", instance_id = ?dev.instance_id, packet = ?packet, transaction_id);
                    match self
                        .handle_packet(packet, dev, conn, transaction_id)
                        .instrument(span)
                        .await
                    {
                        Ok(()) => Ok(()),
                        Err(WorkerError::Packet(err)) => Err(err),
                        Err(err) => return Err(err),
                    }
                }
                Err(err) => Err(err),
            };

            if let Err(err) = r {
                tracelimit::warn_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    transaction_id,
                    instance_id = ?dev.instance_id,
                    "request failed"
                );
                conn.send_completion(transaction_id, &protocol::Status::BAD_DATA, &[])?;
            }
        }
    }

    async fn handle_packet(
        &mut self,
        packet: PacketData,
        dev: &mut VpciChannel,
        conn: &mut Connection<impl RingMem>,
        transaction_id: Option<u64>,
    ) -> Result<(), WorkerError> {
        match packet {
            PacketData::QueryProtocolVersion { .. } => {
                return Err(WorkerError::UnexpectedPacketOrder);
            }
            PacketData::FdoD0Entry { mmio_start } => {
                tracing::trace!(?mmio_start, ?dev.instance_id, "FDO D0 entry");
                dev.config_space.map(mmio_start);
                self.send_device = true;
                // Send the completion after the device has been sent.
                self.send_completion = transaction_id;
            }
            PacketData::FdoD0Exit => {
                tracing::trace!(?dev.instance_id, "FDO D0 exit");
                dev.config_space.unmap();
                conn.send_completion(transaction_id, &protocol::Status::SUCCESS, &[])?;
            }
            PacketData::QueryRelations => {
                self.send_device = true;
                // The protocol does not specify a response, but a VPCI VSC
                // could have set the completion requested bit in the ring
                // buffer packet.
                conn.send_completion(transaction_id, &(), &[])?;
            }
            PacketData::DeviceRequest { slot, request } => {
                if u32::from(slot) != 0 {
                    // FUTURE: support a bus with multiple devices.
                    return Err(PacketError::InvalidSlot(slot).into());
                }
                match request {
                    DeviceRequest::AssignedResources {
                        resources,
                        reply_type,
                    } => {
                        dev.set_bars(&resources.mmio_ranges)
                            .map_err(PacketError::InvalidBars)?;

                        let mut tr = Vec::<u8>::new();
                        dev.map_interrupts(&resources.interrupts, &mut |r| match reply_type {
                            AssignedResourcesReplyType::V1 => {
                                tr.extend(protocol::MsiResource::from(r).as_bytes());
                            }
                            AssignedResourcesReplyType::V2 => {
                                tr.extend(protocol::MsiResource2::from(r).as_bytes());
                            }
                        })
                        .await?;

                        let translated = protocol::DeviceTranslateReply {
                            status: protocol::Status::SUCCESS,
                            slot,
                            mmio_resources: resources.mmio_ranges.map(|r| r.to_protocol()),
                            msi_resource_count: resources.interrupts.len() as u32,
                            reserved: 0,
                        };

                        conn.send_completion(transaction_id, &translated, &tr)?;
                    }
                    DeviceRequest::ReleaseResources => {
                        dev.release_all().await;
                        conn.send_completion(transaction_id, &protocol::Status::SUCCESS, &[])?;
                    }
                    DeviceRequest::CreateInterrupt { interrupt } => {
                        let mut resource = FromZeros::new_zeroed();
                        // TODO: pass failures back the guest, don't fail the channel.
                        dev.map_interrupts(&[interrupt], &mut |r| resource = r)
                            .await?;
                        conn.send_completion(
                            transaction_id,
                            &(protocol::CreateInterruptReply {
                                status: protocol::Status::SUCCESS,
                                rsvd: 0,
                                interrupt: resource,
                            }),
                            &[],
                        )?;
                    }
                    DeviceRequest::DeleteInterrupt { interrupt } => {
                        dev.unmap_interrupt(MsiAddressData {
                            address: interrupt.address,
                            data: interrupt.data_payload,
                        })
                        .await?;
                        conn.send_completion(transaction_id, &protocol::Status::SUCCESS, &[])?;
                    }
                    DeviceRequest::QueryResources => {
                        let reply = protocol::QueryResourceRequirementsReply {
                            status: protocol::Status::SUCCESS,
                            bars: dev.bar_masks,
                        };
                        conn.send_completion(transaction_id, &reply, &[])?;
                    }
                    DeviceRequest::GetResources => {
                        let bars = dev.bars();
                        conn.send_completion(
                            transaction_id,
                            &protocol::PartialResourceList {
                                version: 1,
                                revision: 1,
                                count: 6,
                                descriptors: bars.map(|bar| bar.to_protocol()),
                            },
                            &[],
                        )?;
                    }
                    DeviceRequest::DevicePowerChange { target_state } => {
                        let mut status = protocol::Status::SUCCESS;
                        match target_state {
                            protocol::DevicePowerState::D0 => dev.set_power(true),
                            protocol::DevicePowerState::D3 => dev.set_power(false),
                            _ => status = protocol::Status::BAD_DATA,
                        }
                        conn.send_completion(transaction_id, &status, &[])?;
                    }
                    DeviceRequest::Reset => {
                        conn.send_completion(
                            transaction_id,
                            &protocol::Status::NOT_SUPPORTED,
                            &[],
                        )?;
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
enum InvalidBars {
    #[error("resource {index} was set but corresponds to the high half of a 64-bit bar")]
    ResourceHigh64 { index: usize },
    #[error("resource {index} at {address:#x} was unaligned to {mask:#x}")]
    Unaligned {
        index: usize,
        address: u64,
        mask: u64,
    },
    #[error("resource {index} sized {len:#x} was too large for {mask:#x}")]
    TooLarge { index: usize, len: u64, mask: u64 },
}

impl VpciChannel {
    fn bars(&mut self) -> [MmioResource; 6] {
        if !self.bars_set {
            // Don't return the default BAR state, which would look like
            // everything is mapped at 0.
            return [MmioResource::default(); 6];
        }
        let bars = {
            let mut device = self.device.lock();
            let mut buf = 0;
            [0, 1, 2, 3, 4, 5].map(|i| {
                device
                    .supports_pci()
                    .unwrap()
                    .pci_cfg_read(cfg_space::HeaderType00::BAR0.0 + 4 * i, &mut buf)
                    .now_or_never()
                    .map(|_| buf)
                    .unwrap_or(0)
            })
        };
        let mut resources = [MmioResource::default(); 6];
        for bar in BarMappings::parse(&bars, &self.bar_masks).iter() {
            resources[bar.index as usize] = MmioResource {
                address: bar.base_address,
                len: bar.len,
            };
        }
        tracing::debug!(?resources, "parsed bars");
        resources
    }

    fn set_bars(&mut self, resources: &[MmioResource; 6]) -> Result<(), InvalidBars> {
        let mut bars = [0; 6];
        let mut high64 = false;
        for (i, resource) in resources.iter().enumerate() {
            if resource.len == 0 {
                high64 = false;
                continue;
            }
            if high64 {
                return Err(InvalidBars::ResourceHigh64 { index: i });
            }
            let mut mask = self.bar_masks[i] as u64;
            if cfg_space::BarEncodingBits::from_bits(mask as u32).type_64_bit() {
                high64 = true;
                mask |= (self.bar_masks[i + 1] as u64) << 32;
            }
            if resource.address & !(mask & !0xf) != 0 {
                return Err(InvalidBars::Unaligned {
                    index: i,
                    address: resource.address,
                    mask,
                });
            }
            let bar_len = (!mask | 0xf) + 1;
            if resource.len > bar_len {
                return Err(InvalidBars::TooLarge {
                    index: i,
                    len: resource.len,
                    mask,
                });
            }
            bars[i] = resource.address as u32;
            if high64 {
                bars[i + 1] = (resource.address >> 32) as u32;
            }
        }
        tracing::debug!(?bars, "setting bars");
        {
            let mut device = self.device.lock();
            for (i, bar) in bars.into_iter().enumerate() {
                {
                    device
                        .supports_pci()
                        .unwrap()
                        .pci_cfg_write(cfg_space::HeaderType00::BAR0.0 + 4 * i as u16, bar)
                        .unwrap();
                };
            }
        }
        self.bars_set = true;
        Ok(())
    }

    fn set_power(&mut self, on: bool) {
        let mut device = self.device.lock();
        let mut command = {
            let mut value = 0;
            device
                .supports_pci()
                .unwrap()
                .pci_cfg_read(cfg_space::HeaderType00::STATUS_COMMAND.0, &mut value)
                .now_or_never()
                .map(|_| value)
                .unwrap_or(0)
        };
        let mmio = cfg_space::Command::new()
            .with_mmio_enabled(true)
            .into_bits() as u32;
        if on {
            command |= mmio;
        } else {
            command &= !mmio;
        }
        {
            device
                .supports_pci()
                .unwrap()
                .pci_cfg_write(cfg_space::HeaderType00::STATUS_COMMAND.0, command)
                .unwrap();
        };

        // TODO: set power cap, too, on devices that support it.
    }

    async fn map_interrupts(
        &mut self,
        interrupts: &[InterruptResourceRequest],
        add_resource: &mut (dyn FnMut(protocol::MsiResourceRemapped) + Send),
    ) -> Result<(), PacketError> {
        let interrupts = interrupts.iter().filter(|r| r.vector_count != 0);
        let count = interrupts.clone().count();
        let new_count = self.interrupts.len() + count;
        if new_count > protocol::MAX_SUPPORTED_INTERRUPT_MESSAGES as usize {
            return Err(PacketError::TooManyMsis(new_count as u32));
        }

        for interrupt in interrupts {
            let params = VpciInterruptParameters {
                vector: interrupt.vector,
                multicast: interrupt.delivery_mode == InterruptType::Fixed
                    && interrupt.target_processors.len() > 1,
                target_processors: &interrupt.target_processors,
            };

            let address_data = self
                .msi_mapper
                .register_interrupt(interrupt.vector_count.into(), &params)
                .await
                .map_err(PacketError::RegisterInterrupt)?;

            add_resource(protocol::MsiResourceRemapped {
                reserved: 0,
                message_count: interrupt.vector_count.into(),
                data_payload: address_data.data,
                address: address_data.address,
            });

            tracing::debug!(?address_data, "mapped interrupt");

            self.interrupts.push(address_data);
        }

        Ok(())
    }

    async fn unmap_interrupt(&mut self, interrupt: MsiAddressData) -> Result<(), PacketError> {
        let i = self
            .interrupts
            .iter()
            .position(|x| x == &interrupt)
            .ok_or(PacketError::UnknownInterrupt(interrupt))?;

        self.msi_mapper
            .unregister_interrupt(interrupt.address, interrupt.data)
            .await;
        self.interrupts.swap_remove(i);
        tracing::debug!(?interrupt, "unmapped interrupt");
        Ok(())
    }

    /// Release all resources associated with the device (not the bus).
    async fn release_all(&mut self) {
        // Power off the device.
        self.set_power(false);

        // Unmap all interrupts.
        for MsiAddressData { address, data } in self.interrupts.drain(..) {
            self.msi_mapper.unregister_interrupt(address, data).await;
        }

        // Clear the BARs.
        self.set_bars(&[MmioResource::default(); 6]).unwrap();
        self.bars_set = false;
    }
}

/// Virtual PCI Channel
#[derive(InspectMut)]
pub struct VpciChannel {
    // Runtime services.
    #[inspect(skip)]
    msi_mapper: VpciInterruptMapper,
    #[inspect(skip)]
    config_space: VpciConfigSpace,

    // Static configuration.
    #[inspect(skip)]
    instance_id: Guid,
    serial_num: u32,
    hardware_ids: HardwareIds,
    #[inspect(hex, iter_by_index)]
    bar_masks: [u32; 6],

    // The underlying device.
    #[inspect(skip)]
    device: Arc<CloseableMutex<dyn ChipsetDevice>>,

    // State.
    bars_set: bool,
    #[inspect(iter_by_index)]
    interrupts: Vec<MsiAddressData>,
}

/// Virtual PCI Config Space
#[derive(Inspect)]
#[inspect(skip)]
pub struct VpciConfigSpace {
    offset: VpciConfigSpaceOffset,
    control_mmio: Box<dyn ControlMmioIntercept>,
    vtom: Option<VpciConfigSpaceVtom>,
}

/// The vtom info used by config space.
pub struct VpciConfigSpaceVtom {
    /// The vtom bit.
    pub vtom: u64,
    /// The mmio control region to be registered with vtom.
    pub control_mmio: Box<dyn ControlMmioIntercept>,
}

impl VpciConfigSpace {
    /// Create New PCI Config space.
    pub fn new(
        control_mmio: Box<dyn ControlMmioIntercept>,
        vtom: Option<VpciConfigSpaceVtom>,
    ) -> Self {
        Self {
            offset: VpciConfigSpaceOffset::new(),
            control_mmio,
            vtom,
        }
    }

    /// Returns the offset of the config space
    pub fn offset(&self) -> &VpciConfigSpaceOffset {
        &self.offset
    }

    fn map(&mut self, addr: u64) {
        tracing::trace!(addr, "mapping config space");

        // Remove the vtom bit if set
        let vtom_bit = self.vtom.as_ref().map(|v| v.vtom).unwrap_or(0);
        let addr = addr & !vtom_bit;

        self.offset.0.store(addr, Ordering::Relaxed);
        self.control_mmio.map(addr);

        if let Some(vtom) = self.vtom.as_mut() {
            vtom.control_mmio.map(addr | vtom_bit);
        }
    }

    fn unmap(&mut self) {
        tracing::trace!(
            addr = self.offset.0.load(Ordering::Relaxed),
            "unmapping config space"
        );
        // Note that there may be some current accessors that this will not
        // flush out synchronously. The MMIO implementation in bus.rs must be
        // careful to ignore reads/writes that are not to an expected address.
        //
        // This is idempotent. See [`impl_device_range!`].
        self.control_mmio.unmap();
        if let Some(vtom) = self.vtom.as_mut() {
            vtom.control_mmio.unmap();
        }
        self.offset
            .0
            .store(VpciConfigSpaceOffset::INVALID, Ordering::Relaxed);
    }
}

/// PCI Config space offset structure
#[derive(Debug, Clone, Inspect)]
#[inspect(transparent)]
pub struct VpciConfigSpaceOffset(#[inspect(hex)] Arc<AtomicU64>);

impl VpciConfigSpaceOffset {
    const INVALID: u64 = !0;

    fn new() -> Self {
        Self(Arc::new(Self::INVALID.into()))
    }

    /// PCI Config space offset
    pub fn get(&self) -> Option<u64> {
        let v = self.0.load(Ordering::Relaxed);
        (v != Self::INVALID).then_some(v)
    }
}

impl VpciChannel {
    /// Create New VPCI Channel
    pub(crate) fn new(
        device: &Arc<CloseableMutex<dyn ChipsetDevice>>,
        instance_id: Guid,
        config_space: VpciConfigSpace,
        msi_mapper: VpciInterruptMapper,
    ) -> Result<Self, NotPciDevice> {
        let (hardware_ids, bar_masks);
        {
            let mut device = device.lock();
            let pci = device.supports_pci().ok_or(NotPciDevice)?;
            hardware_ids = pci.probe_hardware_ids();
            bar_masks = pci.probe_bar_masks();
        }

        Ok(VpciChannel {
            msi_mapper,
            config_space,
            instance_id,
            serial_num: instance_id.data1, // Use FIOV precedent of serial number from first block of GUID
            hardware_ids,
            bar_masks,
            device: device.clone(),
            bars_set: false,
            interrupts: Vec::new(),
        })
    }
}

#[async_trait]
impl<M: 'static + Send + Sync + RingMem> SimpleVmbusDevice<M> for VpciChannel {
    type SavedState = NoSavedState;
    type Runner = VpciChannelState<M>;

    fn offer(&self) -> OfferParams {
        OfferParams {
            interface_name: "vpci".to_owned(),
            instance_id: self.instance_id,
            interface_id: protocol::GUID_VPCI_VSP_CHANNEL_TYPE,
            ..Default::default()
        }
    }

    fn inspect(&mut self, req: inspect::Request<'_>, runner: Option<&mut Self::Runner>) {
        let mut resp = req.respond();
        resp.merge(runner).merge(&mut *self);
    }

    fn open(
        &mut self,
        channel: RawAsyncChannel<M>,
        _guest_memory: guestmem::GuestMemory,
    ) -> Result<Self::Runner, ChannelOpenError> {
        Ok(VpciChannelState {
            conn: Connection {
                queue: Queue::new(channel)?,
            },
            state: ProtocolState::Init,
        })
    }

    async fn close(&mut self) {
        self.release_all().await;

        // Unmap the claimed config space. This can also occur if the device sends a D0 exit via the vpci protocol.
        self.config_space.unmap();
    }

    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        worker: &mut Self::Runner,
    ) -> Result<(), Cancelled> {
        let r = stop.until_stopped(worker.run(self)).await?;
        if let Err(err) = r {
            tracing::error!(error = &err as &dyn std::error::Error, "vpci error");
        }
        Ok(())
    }

    fn supports_save_restore(
        &mut self,
    ) -> Option<
        &mut dyn SaveRestoreSimpleVmbusDevice<SavedState = Self::SavedState, Runner = Self::Runner>,
    > {
        None
    }
}

/// Not PCI Device Struct
#[derive(Debug, Error)]
#[error("provided device is not a pci device")]
pub struct NotPciDevice;

#[cfg(test)]
mod tests {
    use super::Connection;
    use super::ProtocolState;
    use super::VpciChannel;
    use super::VpciChannelState;
    use super::VpciConfigSpace;
    use crate::test_helpers::TestVpciInterruptController;
    use chipset_arc_mutex_device::services::MmioInterceptServices;
    use chipset_arc_mutex_device::test_chipset::TestChipset;
    use chipset_device::ChipsetDevice;
    use chipset_device::io::IoResult;
    use chipset_device::mmio::ExternallyManagedMmioIntercepts;
    use chipset_device::mmio::MmioIntercept;
    use chipset_device::mmio::RegisterMmioIntercept;
    use chipset_device::pci::PciConfigSpace;
    use closeable_mutex::CloseableMutex;
    use device_emulators::ReadWriteRequestType;
    use device_emulators::read_as_u32_chunks;
    use device_emulators::write_as_u32_chunks;
    use guestmem::AccessError;
    use guestmem::MemoryRead;
    use guid::Guid;
    use hvdef::HV_PAGE_SIZE;
    use inspect::Inspect;
    use inspect::InspectMut;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::driver::SpawnDriver;
    use pci_core::cfg_space_emu::BarMemoryKind;
    use pci_core::cfg_space_emu::ConfigSpaceType0Emulator;
    use pci_core::cfg_space_emu::DeviceBars;
    use pci_core::chipset_device_ext::PciChipsetDeviceExt;
    use pci_core::msi::MsiConnection;
    use pci_core::spec::hwid::ClassCode;
    use pci_core::spec::hwid::HardwareIds;
    use pci_core::spec::hwid::ProgrammingInterface;
    use pci_core::spec::hwid::Subclass;
    use ring::FlatRingMem;
    use ring::OutgoingPacketType;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;
    use thiserror::Error;
    use vmbus_async::queue::IncomingPacket;
    use vmbus_async::queue::OutgoingPacket;
    use vmbus_async::queue::Queue;
    use vmbus_async::queue::connected_queues;
    use vmbus_ring as ring;
    use vmcore::vpci_msi::VpciInterruptMapper;
    use vpci_protocol as protocol;
    use vpci_protocol::SlotNumber;
    use zerocopy::FromBytes;
    use zerocopy::Immutable;
    use zerocopy::IntoBytes;
    use zerocopy::KnownLayout;

    enum ReadPacketInfo {
        None,
        NewTransaction,
        Completion(u64),
    }

    struct MockVpciGuestDevice {
        config: HardwareIds,
        host_queue: Queue<FlatRingMem>,
        transaction_id: AtomicU64,
        protocol_version: protocol::ProtocolVersion,
    }

    fn connected_device(
        driver: &impl SpawnDriver,
        device: Arc<CloseableMutex<dyn ChipsetDevice>>,
        msi_mapper: Arc<TestVpciInterruptController>,
    ) -> MockVpciGuestDevice {
        let (host, guest) = connected_queues(16384);
        let (hardware_ids, bar_masks);
        {
            let mut device = device.lock();
            let pci = device.supports_pci().unwrap();
            hardware_ids = pci.probe_hardware_ids();
            bar_masks = pci.probe_bar_masks();
        }
        let config_space = VpciConfigSpace::new(
            ExternallyManagedMmioIntercepts.new_io_region("test", 2 * HV_PAGE_SIZE),
            None,
        );
        let mut state = VpciChannel {
            msi_mapper: VpciInterruptMapper::new(msi_mapper),
            config_space,
            instance_id: Guid::new_random(),
            serial_num: 0x1234,
            hardware_ids,
            bar_masks,
            device,
            bars_set: false,
            interrupts: Vec::new(),
        };
        let mut worker = VpciChannelState {
            conn: Connection { queue: host },
            state: ProtocolState::Init,
        };
        driver
            .spawn("worker", async move { worker.run(&mut state).await })
            .detach();
        MockVpciGuestDevice::new(guest, 0, hardware_ids)
    }

    #[derive(Debug, Error)]
    enum GuestError {
        #[error("queue error")]
        Queue(#[source] vmbus_async::queue::Error),
        #[error("guest memory access error")]
        Access(#[source] AccessError),
    }

    #[repr(C)]
    #[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
    struct Relations2 {
        header: protocol::QueryBusRelations2,
        device: protocol::DeviceDescription2,
    }

    impl MockVpciGuestDevice {
        fn new(queue: Queue<FlatRingMem>, _index: usize, config: HardwareIds) -> Self {
            Self {
                config,
                host_queue: queue,
                transaction_id: AtomicU64::new(1),
                protocol_version: protocol::ProtocolVersion::VB,
            }
        }

        async fn read_packet<T: IntoBytes + FromBytes + Immutable + KnownLayout>(
            &mut self,
            pkt_info: &mut ReadPacketInfo,
        ) -> Result<T, GuestError> {
            let mut queue = self.host_queue.split().0;
            let packet = queue.read().await.map_err(GuestError::Queue)?;
            match &*packet {
                IncomingPacket::Data(packet) => {
                    let result = packet.reader().read_plain().map_err(GuestError::Access)?;
                    *pkt_info = ReadPacketInfo::NewTransaction;
                    Ok(result)
                }
                IncomingPacket::Completion(completion) => {
                    let result: T = completion
                        .reader()
                        .read_plain()
                        .map_err(GuestError::Access)?;
                    *pkt_info = ReadPacketInfo::Completion(completion.transaction_id());
                    Ok(result)
                }
            }
        }

        async fn write_packet<T: IntoBytes + Immutable + KnownLayout>(
            &mut self,
            transaction_id: Option<u64>,
            payload: &T,
        ) -> Result<(), GuestError> {
            self.host_queue
                .split()
                .1
                .write(OutgoingPacket {
                    transaction_id: transaction_id.unwrap_or(0),
                    packet_type: if transaction_id.is_some() {
                        OutgoingPacketType::InBandWithCompletion
                    } else {
                        OutgoingPacketType::InBandNoCompletion
                    },
                    payload: &[payload.as_bytes()],
                })
                .await
                .map_err(GuestError::Queue)
        }

        async fn negotiate_version(&mut self) {
            if let Err(vsp_version) = self.try_negotiate_version().await {
                self.protocol_version = vsp_version;
                self.try_negotiate_version().await.unwrap();
            }
        }

        async fn try_negotiate_version(&mut self) -> Result<(), protocol::ProtocolVersion> {
            let query_version = protocol::QueryProtocolVersion {
                message_type: protocol::MessageType::QUERY_PROTOCOL_VERSION,
                protocol_version: self.protocol_version,
            };
            let transaction_id = self.transaction_id.fetch_add(1, Ordering::Relaxed);
            self.write_packet(Some(transaction_id), &query_version)
                .await
                .unwrap();

            let mut pkt_info = ReadPacketInfo::None;
            let reply: protocol::QueryProtocolVersionReply =
                self.read_packet(&mut pkt_info).await.unwrap();
            if let ReadPacketInfo::Completion(id) = pkt_info {
                assert_eq!(id, transaction_id);
                if reply.status == protocol::Status::SUCCESS {
                    assert_eq!(reply.protocol_version, self.protocol_version);
                    Ok(())
                } else {
                    Err(reply.protocol_version)
                }
            } else {
                panic!("Unexpected version reply")
            }
        }

        async fn initiate_power_on(&mut self, base_address: u64) -> u64 {
            let power_on = protocol::FdoD0Entry {
                message_type: protocol::MessageType::FDO_D0_ENTRY,
                padding: 0,
                mmio_start: base_address,
            };
            let transaction_id = self.transaction_id.fetch_add(1, Ordering::Relaxed);
            self.write_packet(Some(transaction_id), &power_on)
                .await
                .unwrap();
            transaction_id
        }

        fn verify_device_relations2(&self, message: &Relations2) {
            let relations = &message.header;
            let device = &message.device;
            assert_eq!(relations.device_count, 1);
            assert_eq!(device.pnp_id.vendor_id, self.config.vendor_id);
            assert_eq!(device.pnp_id.device_id, self.config.device_id);
            assert_eq!(device.pnp_id.revision_id, self.config.revision_id);
            assert_eq!(device.pnp_id.prog_if, u8::from(self.config.prog_if));
            assert_eq!(device.pnp_id.sub_class, u8::from(self.config.sub_class));
            assert_eq!(device.pnp_id.base_class, u8::from(self.config.base_class));
            assert_eq!(device.pnp_id.sub_vendor_id, self.config.type0_sub_vendor_id);
            assert_eq!(device.pnp_id.sub_system_id, self.config.type0_sub_system_id);
            assert_eq!(device.slot, SlotNumber::new());
            assert_eq!(device.flags, 0,);
            assert_eq!(device.numa_node, 0);
            assert_eq!(device.rsvd, 0);
        }

        async fn start_device(&mut self, base_address: u64) {
            self.negotiate_version().await;
            let transaction_id = self.initiate_power_on(base_address).await;
            let mut pkt_info = ReadPacketInfo::None;
            let relations: Relations2 = self.read_packet(&mut pkt_info).await.unwrap();
            if let ReadPacketInfo::NewTransaction = pkt_info {
                assert_eq!(
                    relations.header.message_type,
                    protocol::MessageType::BUS_RELATIONS2
                );
                self.verify_device_relations2(&relations);
            } else {
                panic!("Expecting QueryBusRelations2 message in response to version.");
            }

            let mut pkt_info = ReadPacketInfo::None;
            let status: protocol::Status = self.read_packet(&mut pkt_info).await.unwrap();
            if let ReadPacketInfo::Completion(id) = pkt_info {
                assert_eq!(id, transaction_id);
                assert_eq!(status, protocol::Status::SUCCESS);
            } else {
                panic!("Unexpected D0 (power on) reply");
            }
        }

        // returns MSI address and data
        async fn register_interrupt(
            &mut self,
            vector: u8,
            target_processors: &[u16],
        ) -> (u64, u32) {
            let mut interrupt = protocol::MsiResourceDescriptor2 {
                vector,
                delivery_mode: protocol::DeliveryMode::FIXED,
                vector_count: 1,
                processor_count: target_processors.len() as u16,
                processor_array: Default::default(),
                reserved: 0,
            };
            interrupt.processor_array[..target_processors.len()].copy_from_slice(target_processors);
            let interrupt = protocol::CreateInterrupt2 {
                message_type: protocol::MessageType::CREATE_INTERRUPT2,
                slot: SlotNumber::new(),
                interrupt,
            };
            let transaction_id = self.transaction_id.fetch_add(1, Ordering::Relaxed);
            self.write_packet(Some(transaction_id), &interrupt)
                .await
                .unwrap();

            let mut pkt_info = ReadPacketInfo::None;
            let reply: protocol::CreateInterruptReply =
                self.read_packet(&mut pkt_info).await.unwrap();
            if let ReadPacketInfo::Completion(id) = pkt_info {
                assert_eq!(id, transaction_id);
                assert_eq!(reply.status, protocol::Status::SUCCESS);
            } else {
                panic!("Unexpected CreateInterrupt2 reply");
            }
            assert_eq!(reply.rsvd, 0);
            assert_eq!(reply.interrupt.message_count, 1);
            (reply.interrupt.address, reply.interrupt.data_payload)
        }
    }

    struct NullDevice {
        config_space: ConfigSpaceType0Emulator,
    }

    impl Inspect for NullDevice {
        fn inspect(&self, req: inspect::Request<'_>) {
            req.ignore();
        }
    }

    impl InspectMut for NullDevice {
        fn inspect_mut(&mut self, req: inspect::Request<'_>) {
            req.ignore();
        }
    }

    impl ChipsetDevice for NullDevice {
        fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
            Some(self)
        }

        fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
            Some(self)
        }
    }

    impl MmioIntercept for NullDevice {
        fn mmio_read(&mut self, _address: u64, _data: &mut [u8]) -> IoResult {
            IoResult::Ok
        }
        fn mmio_write(&mut self, _address: u64, _data: &[u8]) -> IoResult {
            IoResult::Ok
        }
    }

    impl PciConfigSpace for NullDevice {
        fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
            self.config_space.read_u32(offset, value)
        }

        fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
            self.config_space.write_u32(offset, value)
        }
    }

    #[async_test]
    async fn verify_simple_device(driver: DefaultDriver) {
        let msi_controller = TestVpciInterruptController::new();
        let pci_config = HardwareIds {
            vendor_id: 0x123,
            device_id: 0x789,
            revision_id: 1,
            prog_if: ProgrammingInterface::NONE,
            base_class: ClassCode::BASE_SYSTEM_PERIPHERAL,
            sub_class: Subclass::BASE_SYSTEM_PERIPHERAL_OTHER,
            type0_sub_vendor_id: 0x456,
            type0_sub_system_id: 0x1,
        };

        let pci = Arc::new(CloseableMutex::new(NullDevice {
            config_space: ConfigSpaceType0Emulator::new(pci_config, Vec::new(), DeviceBars::new()),
        }));
        let mut guest_driver = connected_device(&driver, pci.clone(), msi_controller);
        let base_address = 0x140000000;
        guest_driver.start_device(base_address).await;
    }

    #[async_test]
    async fn verify_version_negotiation(driver: DefaultDriver) {
        let msi_controller = TestVpciInterruptController::new();
        let pci_config = HardwareIds {
            vendor_id: 0x123,
            device_id: 0x789,
            revision_id: 1,
            prog_if: ProgrammingInterface::NONE,
            base_class: ClassCode::BASE_SYSTEM_PERIPHERAL,
            sub_class: Subclass::BASE_SYSTEM_PERIPHERAL_OTHER,
            type0_sub_vendor_id: 0x456,
            type0_sub_system_id: 0x1,
        };
        let pci = Arc::new(CloseableMutex::new(NullDevice {
            config_space: ConfigSpaceType0Emulator::new(pci_config, Vec::new(), DeviceBars::new()),
        }));
        let mut guest_driver = connected_device(&driver, pci.clone(), msi_controller);
        guest_driver.protocol_version = protocol::ProtocolVersion(0x00020000);
        let base_address = 0x140000000;
        guest_driver.start_device(base_address).await;
    }

    #[async_test]
    async fn verify_simple_capability(driver: DefaultDriver) {
        let msi_conn = MsiConnection::new();
        let pci_config = HardwareIds {
            vendor_id: 0x123,
            device_id: 0x789,
            revision_id: 1,
            prog_if: ProgrammingInterface::NONE,
            base_class: ClassCode::BASE_SYSTEM_PERIPHERAL,
            sub_class: Subclass::BASE_SYSTEM_PERIPHERAL_OTHER,
            type0_sub_vendor_id: 0x456,
            type0_sub_system_id: 0x1,
        };
        let (_msix, msix_capability) =
            pci_core::capabilities::msix::MsixEmulator::new(0, 64, msi_conn.target());

        let msi_controller = TestVpciInterruptController::new();
        msi_conn.connect(msi_controller.signal_msi());

        let pci = Arc::new(CloseableMutex::new(NullDevice {
            config_space: ConfigSpaceType0Emulator::new(
                pci_config,
                vec![Box::new(msix_capability)],
                DeviceBars::new(),
            ),
        }));
        let mut guest_driver = connected_device(&driver, pci.clone(), msi_controller);
        let base_address = 0x120000000;
        guest_driver.start_device(base_address).await;
    }

    #[async_test]
    async fn verify_mmio(driver: DefaultDriver) {
        let msi_controller = TestVpciInterruptController::new();
        let pci_config = HardwareIds {
            vendor_id: 0x123,
            device_id: 0x789,
            revision_id: 1,
            prog_if: ProgrammingInterface::NONE,
            base_class: ClassCode::BASE_SYSTEM_PERIPHERAL,
            sub_class: Subclass::BASE_SYSTEM_PERIPHERAL_OTHER,
            type0_sub_vendor_id: 0x456,
            type0_sub_system_id: 0x1,
        };

        let pci = Arc::new(CloseableMutex::new(NullDevice {
            config_space: ConfigSpaceType0Emulator::new(
                pci_config,
                Vec::new(),
                DeviceBars::new().bar0(0x1000, BarMemoryKind::Dummy),
            ),
        }));
        let mut guest_driver = connected_device(&driver, pci.clone(), msi_controller);

        let base_address = 0x80000000;
        guest_driver.start_device(base_address).await;
        let mut pci = pci.lock();
        for i in 0..6 {
            pci.pci_cfg_write(0x10 + 4 * i, 0xffffffff).unwrap();
        }

        let mut value = 0;
        pci.pci_cfg_read(0x10, &mut value).unwrap();
        assert_eq!(value, 0xfffff004);
        pci.pci_cfg_read(0x14, &mut value).unwrap();
        assert_eq!(value, 0xffffffff);
        pci.pci_cfg_read(0x18, &mut value).unwrap();
        assert_eq!(value, 0);
        pci.pci_cfg_read(0x1c, &mut value).unwrap();
        assert_eq!(value, 0);
        pci.pci_cfg_read(0x20, &mut value).unwrap();
        assert_eq!(value, 0);
        pci.pci_cfg_read(0x24, &mut value).unwrap();
        assert_eq!(value, 0);

        pci.pci_cfg_write(0x14, 0x20).unwrap();
        pci.pci_cfg_write(0x10, 0x0).unwrap();
        pci.pci_cfg_read(0x10, &mut value).unwrap();
        assert_eq!(value, 0x4);
        pci.pci_cfg_read(0x14, &mut value).unwrap();
        assert_eq!(value, 0x20);

        pci.pci_cfg_write(
            0x4,
            pci_core::spec::cfg_space::Command::new()
                .with_mmio_enabled(true)
                .into_bits() as u32,
        )
        .unwrap();

        // Writes to BAR address are not allowed once MMIO is enabled.
        pci.pci_cfg_write(0x14, 0xffffffff).unwrap();
        pci.pci_cfg_write(0x10, 0xffffffff).unwrap();
        pci.pci_cfg_read(0x10, &mut value).unwrap();
        assert_eq!(value, 0x4);
        pci.pci_cfg_read(0x14, &mut value).unwrap();
        assert_eq!(value, 0x20);
    }

    #[async_test]
    async fn verify_simple_device_registers(driver: DefaultDriver) {
        let msi_controller = TestVpciInterruptController::new();

        struct TestDevice(ConfigSpaceType0Emulator);
        impl TestDevice {
            fn new(register_mmio: &mut dyn RegisterMmioIntercept) -> Self {
                Self(ConfigSpaceType0Emulator::new(
                    HardwareIds {
                        vendor_id: 0x123,
                        device_id: 0x789,
                        revision_id: 1,
                        prog_if: ProgrammingInterface::NONE,
                        base_class: ClassCode::BASE_SYSTEM_PERIPHERAL,
                        sub_class: Subclass::BASE_SYSTEM_PERIPHERAL_OTHER,
                        type0_sub_vendor_id: 0x456,
                        type0_sub_system_id: 0x1,
                    },
                    Vec::new(),
                    DeviceBars::new()
                        .bar0(
                            0x1000,
                            BarMemoryKind::Intercept(register_mmio.new_io_region("bar0", 0x1000)),
                        )
                        .bar2(
                            0x2000,
                            BarMemoryKind::Intercept(register_mmio.new_io_region("bar2", 0x2000)),
                        ),
                ))
            }

            fn read_bar_u32(&self, bar: u8, offset: u16) -> u32 {
                if bar == 0 && offset == 0 {
                    1
                } else if bar == 0 && offset == 4 {
                    2
                } else if bar == 2 && offset == 0 {
                    3
                } else if bar == 2 && offset == HV_PAGE_SIZE as u16 {
                    4
                } else {
                    panic!("Unexpected address {}/{:#x}", bar, offset);
                }
            }

            fn write_bar_u32(&mut self, bar: u8, offset: u16, val: u32) {
                if bar == 0 && offset == 0 {
                    assert_eq!(val, 1);
                } else if bar == 0 && offset == 4 {
                    assert_eq!(val, 2);
                } else if bar == 2 && offset == 0 {
                    assert_eq!(val, 3);
                } else if bar == 2 && offset == HV_PAGE_SIZE as u16 {
                    assert_eq!(val, 4);
                } else {
                    panic!("Unexpected address {}/{:#x}", bar, offset);
                }
            }
        }

        impl InspectMut for TestDevice {
            fn inspect_mut(&mut self, req: inspect::Request<'_>) {
                req.ignore();
            }
        }

        impl Inspect for TestDevice {
            fn inspect(&self, req: inspect::Request<'_>) {
                req.ignore();
            }
        }

        impl ChipsetDevice for TestDevice {
            fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
                Some(self)
            }

            fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
                Some(self)
            }
        }

        impl MmioIntercept for TestDevice {
            fn mmio_read(&mut self, address: u64, data: &mut [u8]) -> IoResult {
                if let Some((bar, offset)) = self.0.find_bar(address) {
                    read_as_u32_chunks(offset, data, |offset| self.read_bar_u32(bar, offset))
                }
                IoResult::Ok
            }

            fn mmio_write(&mut self, address: u64, data: &[u8]) -> IoResult {
                if let Some((bar, offset)) = self.0.find_bar(address) {
                    write_as_u32_chunks(offset, data, |offset, request_type| match request_type {
                        ReadWriteRequestType::Write(value) => {
                            self.write_bar_u32(bar, offset, value);
                            None
                        }
                        ReadWriteRequestType::Read => Some(self.read_bar_u32(bar, offset)),
                    })
                }
                IoResult::Ok
            }
        }

        impl PciConfigSpace for TestDevice {
            fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
                self.0.read_u32(offset, value)
            }
            fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
                self.0.write_u32(offset, value)
            }
        }

        let vm_chipset = TestChipset::default();
        let pci = vm_chipset
            .device_builder("test")
            .with_external_pci()
            .add(|services| TestDevice::new(&mut services.register_mmio()))
            .unwrap();
        let mut guest_driver = connected_device(&driver, pci.clone(), msi_controller);
        let base_address = 0x1000000;
        guest_driver.start_device(base_address).await;

        let write_u32 = |address, value: u32| {
            assert!(
                vm_chipset
                    .mmio_write(address, &value.to_ne_bytes())
                    .is_some()
            );
        };
        let read_u32 = |address| {
            let mut value = [0; 4];
            assert!(vm_chipset.mmio_read(address, &mut value).is_some());
            u32::from_ne_bytes(value)
        };

        let bar_address1 = 0x2000000000;
        pci.lock()
            .pci_cfg_write(0x14, u32::try_from(bar_address1 >> 32).unwrap())
            .unwrap();
        pci.lock()
            .pci_cfg_write(0x10, u32::try_from(bar_address1 & 0xffffffff).unwrap())
            .unwrap();

        let bar_address2: u64 = 0x4000;
        pci.lock()
            .pci_cfg_write(0x1c, u32::try_from(bar_address2 >> 32).unwrap())
            .unwrap();
        pci.lock()
            .pci_cfg_write(0x18, u32::try_from(bar_address2 & 0xffffffff).unwrap())
            .unwrap();

        pci.lock()
            .pci_cfg_write(
                0x4,
                pci_core::spec::cfg_space::Command::new()
                    .with_mmio_enabled(true)
                    .into_bits() as u32,
            )
            .unwrap();

        assert_eq!(read_u32(bar_address1), 1);
        assert_eq!(read_u32(bar_address1 + 4), 2);
        assert_eq!(read_u32(bar_address2), 3);
        assert_eq!(read_u32(bar_address2 + HV_PAGE_SIZE), 4);
        write_u32(bar_address1, 1);
        write_u32(bar_address1 + 4, 2);
        write_u32(bar_address2, 3);
        write_u32(bar_address2 + HV_PAGE_SIZE, 4);
    }

    #[async_test]
    async fn verify_simple_device_interrupt(driver: DefaultDriver) {
        let msi_controller = TestVpciInterruptController::new();
        let pci_config = HardwareIds {
            vendor_id: 0x123,
            device_id: 0x789,
            revision_id: 1,
            prog_if: ProgrammingInterface::NONE,
            base_class: ClassCode::BASE_SYSTEM_PERIPHERAL,
            sub_class: Subclass::BASE_SYSTEM_PERIPHERAL_OTHER,
            type0_sub_vendor_id: 0x456,
            type0_sub_system_id: 0x1,
        };

        let pci = Arc::new(CloseableMutex::new(NullDevice {
            config_space: ConfigSpaceType0Emulator::new(pci_config, Vec::new(), DeviceBars::new()),
        }));
        let mut guest_driver = connected_device(&driver, pci.clone(), msi_controller);
        let base_address = 0x1000000;
        guest_driver.start_device(base_address).await;
        let target_processors = vec![1];
        let (addr, data) = guest_driver
            .register_interrupt(0x13, &target_processors)
            .await;
        assert_ne!(addr, 0);
        assert_eq!(data, 0);
    }
}
