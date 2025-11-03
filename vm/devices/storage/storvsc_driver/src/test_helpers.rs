// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers for unit tests.

#![cfg_attr(not(test), expect(dead_code))]
#![expect(missing_docs)]

use crate::PacketError;
use crate::Storvsc;
use crate::StorvscCompletion;
use crate::StorvscError;
use crate::StorvscErrorInner;
use crate::StorvscRequest;
use crate::StorvscState;
use futures::FutureExt;
use futures_concurrency::future::Race;
use guestmem::GuestMemory;
use guestmem::MemoryRead;
use guestmem::ranges::PagedRange;
use inspect::Inspect;
use mesh_channel::Receiver;
use mesh_channel::RecvError;
use mesh_channel::Sender;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::timer::PolledTimer;
use scsi_buffers::RequestBuffers;
use std::future::poll_fn;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::time;
use task_control::TaskControl;
use thiserror::Error;
use vmbus_async::queue;
use vmbus_async::queue::IncomingPacket;
use vmbus_async::queue::OutgoingPacket;
use vmbus_async::queue::PacketRef;
use vmbus_async::queue::Queue;
use vmbus_channel::RawAsyncChannel;
use vmbus_ring::FlatRingMem;
use vmbus_ring::OutgoingPacketType;
use vmbus_ring::RingMem;
use vmbus_ring::gparange::MultiPagedRangeBuf;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

const MAX_VMBUS_PACKET_SIZE: usize = vmbus_ring::PacketSize::in_band(
    size_of::<storvsp_protocol::Packet>() + storvsp_protocol::SCSI_REQUEST_LEN_MAX,
);

#[derive(Debug)]
struct StorvspPacket {
    data: StorvspPacketData,
    transaction_id: u64,
    request_size: usize,
}

#[derive(Debug, Clone)]
enum StorvspPacketData {
    BeginInitialization,
    EndInitialization,
    QueryProtocolVersion(u16),
    QueryProperties,
    CreateSubChannels(u16),
    ExecuteScsi(Arc<ScsiRequestAndRange>),
    ResetBus,
    ResetAdapter,
    ResetLun,
}

#[repr(u16)]
#[derive(Copy, Clone, Debug, Inspect, PartialEq, Eq, PartialOrd, Ord)]
enum Version {
    Win6 = storvsp_protocol::VERSION_WIN6,
    Win7 = storvsp_protocol::VERSION_WIN7,
    Win8 = storvsp_protocol::VERSION_WIN8,
    Blue = storvsp_protocol::VERSION_BLUE,
}

#[derive(Debug, Error)]
#[error("protocol version {0:#x} not supported")]
struct UnsupportedVersion(u16);

impl Version {
    fn parse(major_minor: u16) -> Result<Self, UnsupportedVersion> {
        let version = match major_minor {
            storvsp_protocol::VERSION_WIN6 => Self::Win6,
            storvsp_protocol::VERSION_WIN7 => Self::Win7,
            storvsp_protocol::VERSION_WIN8 => Self::Win8,
            storvsp_protocol::VERSION_BLUE => Self::Blue,
            version => return Err(UnsupportedVersion(version)),
        };
        assert_eq!(version as u16, major_minor);
        Ok(version)
    }

    fn max_request_size(&self) -> usize {
        match self {
            Version::Win8 | Version::Blue => storvsp_protocol::SCSI_REQUEST_LEN_V2,
            Version::Win6 | Version::Win7 => storvsp_protocol::SCSI_REQUEST_LEN_V1,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct Range {
    buf: MultiPagedRangeBuf,
    len: usize,
    is_write: bool,
}

impl Range {
    fn new(buf: MultiPagedRangeBuf, request: &storvsp_protocol::ScsiRequest) -> Option<Self> {
        let len = request.data_transfer_length as usize;
        let is_write = request.data_in != 0;
        // Ensure there is exactly one range and it's large enough, or there are
        // zero ranges and there is no associated SCSI buffer.
        if buf.range_count() > 1 || (len > 0 && buf.first()?.len() < len) {
            return None;
        }
        Some(Self { buf, len, is_write })
    }

    #[expect(dead_code)]
    fn buffer<'a>(&'a self, guest_memory: &'a GuestMemory) -> RequestBuffers<'a> {
        let mut range = self.buf.first().unwrap_or_else(PagedRange::empty);
        range.truncate(self.len);
        RequestBuffers::new(guest_memory, range, self.is_write)
    }
}

#[derive(Debug, Clone)]
#[expect(dead_code)]
pub(crate) struct ScsiRequestAndRange {
    external_data: Range,
    request: storvsp_protocol::ScsiRequest,
    request_size: usize,
}

fn parse_storvsp_packet<T: RingMem>(
    packet: &IncomingPacket<'_, T>,
    pool: &mut Vec<Arc<ScsiRequestAndRange>>,
) -> Result<StorvspPacket, PacketError> {
    let packet = match packet {
        IncomingPacket::Completion(_) => return Err(PacketError::InvalidPacketType),
        IncomingPacket::Data(packet) => packet,
    };
    let transaction_id = packet
        .transaction_id()
        .ok_or(PacketError::NotTransactional)?;

    let mut reader = packet.reader();
    let header: storvsp_protocol::Packet = reader.read_plain().map_err(PacketError::Access)?;
    // You would expect that this should be limited to the current protocol
    // version's maximum packet size, but this is not what Hyper-V does, and
    // Linux 6.1 relies on this behavior during protocol initialization.
    let request_size = reader.len().min(storvsp_protocol::SCSI_REQUEST_LEN_MAX);
    let data = match header.operation {
        storvsp_protocol::Operation::BEGIN_INITIALIZATION => StorvspPacketData::BeginInitialization,
        storvsp_protocol::Operation::END_INITIALIZATION => StorvspPacketData::EndInitialization,
        storvsp_protocol::Operation::QUERY_PROTOCOL_VERSION => {
            let mut version = storvsp_protocol::ProtocolVersion::new_zeroed();
            reader
                .read(version.as_mut_bytes())
                .map_err(PacketError::Access)?;
            StorvspPacketData::QueryProtocolVersion(version.major_minor)
        }
        storvsp_protocol::Operation::QUERY_PROPERTIES => StorvspPacketData::QueryProperties,
        storvsp_protocol::Operation::EXECUTE_SRB => {
            let mut full_request = pool.pop().unwrap_or_else(|| {
                Arc::new(ScsiRequestAndRange {
                    external_data: Range::default(),
                    request: storvsp_protocol::ScsiRequest::new_zeroed(),
                    request_size,
                })
            });

            {
                let full_request = Arc::get_mut(&mut full_request).unwrap();
                let request_buf = &mut full_request.request.as_mut_bytes()[..request_size];
                reader.read(request_buf).map_err(PacketError::Access)?;

                let mut buf = MultiPagedRangeBuf::new();
                packet
                    .read_external_ranges(&mut buf)
                    .map_err(PacketError::Range)?;

                full_request.external_data = Range::new(buf, &full_request.request)
                    .ok_or(PacketError::InvalidDataTransferLength)?;
            }

            StorvspPacketData::ExecuteScsi(full_request)
        }
        storvsp_protocol::Operation::RESET_LUN => StorvspPacketData::ResetLun,
        storvsp_protocol::Operation::RESET_ADAPTER => StorvspPacketData::ResetAdapter,
        storvsp_protocol::Operation::RESET_BUS => StorvspPacketData::ResetBus,
        storvsp_protocol::Operation::CREATE_SUB_CHANNELS => {
            let mut sub_channel_count: u16 = 0;
            reader
                .read(sub_channel_count.as_mut_bytes())
                .map_err(PacketError::Access)?;
            StorvspPacketData::CreateSubChannels(sub_channel_count)
        }
        _ => return Err(PacketError::UnrecognizedOperation(header.operation)),
    };

    Ok(StorvspPacket {
        data,
        request_size,
        transaction_id,
    })
}

pub struct TestStorvscWorker<T: Send + Sync + RingMem> {
    task: TaskControl<StorvscState, Storvsc<T>>,
    new_request_sender: Option<Sender<StorvscRequest>>,
}

impl<T: 'static + Send + Sync + RingMem> TestStorvscWorker<T> {
    pub fn new() -> Self {
        Self {
            task: TaskControl::new(StorvscState),
            new_request_sender: None,
        }
    }

    pub fn start(&mut self, spawner: impl Spawn, channel: RawAsyncChannel<T>) {
        let (new_request_sender, new_request_receiver) = mesh_channel::channel::<StorvscRequest>();
        let storvsc = Storvsc::new(
            channel,
            storvsp_protocol::ProtocolVersion {
                major_minor: storvsp_protocol::VERSION_BLUE,
                reserved: 0,
            },
            new_request_receiver,
        )
        .unwrap();
        self.new_request_sender = Some(new_request_sender);

        self.task.insert(spawner, "storvsc", storvsc);
        self.task.start();
    }

    pub async fn stop(&mut self) {
        self.task.stop().await;
    }

    pub async fn resume(&mut self) {
        self.task.start();
    }

    pub(crate) fn get_mut(&mut self) -> &Storvsc<T> {
        self.task.get_mut().1.unwrap()
    }

    pub async fn teardown(&mut self) {
        self.task.stop().await;
        self.task.remove();
    }

    /// Waits for negotiation to complete or panics.
    pub async fn wait_for_negotiation(
        &mut self,
        timer: &mut PolledTimer,
        negotiation_timeout_millis: u64,
    ) {
        // Wait until storvsc is ready and negotiated, max of 1 second.
        let mut has_negotiated = false;
        let interval_millis = 100;
        for _ in 0..(negotiation_timeout_millis / interval_millis) {
            timer
                .sleep(time::Duration::from_millis(interval_millis))
                .await;
            self.task.stop().await;
            has_negotiated = self.task.state().unwrap().has_negotiated;
            self.task.start();
            if has_negotiated {
                break;
            }
        }
        if !has_negotiated {
            panic!("storvsc negotiation did not complete within timeout");
        }
    }

    /// Send a SCSI request to storvsp over VMBus.
    pub async fn send_request(
        &mut self,
        request: &storvsp_protocol::ScsiRequest,
        buf_gpa: u64,
        byte_len: usize,
    ) -> Result<storvsp_protocol::ScsiRequest, StorvscError> {
        let (sender, mut receiver) = mesh_channel::channel::<StorvscCompletion>();
        let storvsc_request = StorvscRequest {
            request: *request,
            buf_gpa,
            byte_len,
            completion_sender: sender,
        };
        match &self.new_request_sender {
            Some(request_sender) => {
                request_sender.send(storvsc_request);
                Ok(())
            }
            None => Err(StorvscError(StorvscErrorInner::Uninitialized)),
        }?;

        let resp = receiver
            .recv()
            .await
            .map_err(|err| StorvscError(StorvscErrorInner::CompletionError(err)))?;

        if resp.completion.is_some() {
            Ok(resp.completion.unwrap())
        } else {
            Err(StorvscError(StorvscErrorInner::Cancelled))
        }
    }
}

pub(crate) struct TestStorvspWorker {
    task: Task<()>,
    command_request_sender: Sender<TestStorvspCommandRequest>,
}

struct TestStorvsp {
    _mem: GuestMemory,
    queue: Queue<FlatRingMem>,
    full_request_pool: Vec<Arc<ScsiRequestAndRange>>,
    version: storvsp_protocol::ProtocolVersion,
    subchannel_count: u16,
    command_request_receiver: Receiver<TestStorvspCommandRequest>,
    inner: TestStorvspInner,
}

struct TestStorvspInner {
    request_size: usize,
}

pub(crate) struct TestStorvspCommandRequest {
    packet: storvsp_protocol::Packet,
    transaction_id: u64,
    requires_completion: bool,
    payload: [u8; storvsp_protocol::SCSI_REQUEST_LEN_MAX],
    payload_size: usize,
}

impl TestStorvspWorker {
    pub fn start(
        spawner: impl Spawn,
        mem: GuestMemory,
        queue: Queue<FlatRingMem>,
        full_request_pool: Vec<Arc<ScsiRequestAndRange>>,
    ) -> Self {
        let (command_request_sender, command_request_receiver) =
            mesh_channel::channel::<TestStorvspCommandRequest>();
        let task = spawner.spawn("test_storvsp", async move {
            let mut worker =
                TestStorvsp::new(mem, queue, full_request_pool, command_request_receiver);
            worker.run().await;
        });

        Self {
            task,
            command_request_sender,
        }
    }

    pub async fn teardown(self) {
        self.task.cancel().await;
    }

    pub fn send_vmbus_data_packet_no_completion<P: IntoBytes + Immutable + KnownLayout>(
        &mut self,
        packet: storvsp_protocol::Packet,
        transaction_id: u64,
        payload: &P,
    ) {
        let payload_bytes_slice = payload.as_bytes();
        let mut payload_bytes = [0_u8; storvsp_protocol::SCSI_REQUEST_LEN_MAX];
        payload_bytes[..payload_bytes_slice.len()].clone_from_slice(payload_bytes_slice);
        self.command_request_sender.send(TestStorvspCommandRequest {
            packet,
            transaction_id,
            requires_completion: false,
            payload: payload_bytes,
            payload_size: payload_bytes_slice.len(),
        })
    }
}

impl TestStorvsp {
    fn new(
        mem: GuestMemory,
        queue: Queue<FlatRingMem>,
        full_request_pool: Vec<Arc<ScsiRequestAndRange>>,
        command_request_receiver: Receiver<TestStorvspCommandRequest>,
    ) -> Self {
        TestStorvsp {
            _mem: mem,
            queue,
            full_request_pool,
            subchannel_count: 0,
            version: storvsp_protocol::ProtocolVersion {
                major_minor: 0,
                reserved: 0,
            },
            command_request_receiver,
            inner: TestStorvspInner {
                request_size: storvsp_protocol::SCSI_REQUEST_LEN_V1,
            },
        }
    }

    pub async fn run(&mut self) {
        self.negotiate().await.unwrap();
        self.process_packets().await.unwrap(); // It's normal to exit here when the channel closes
        tracing::error!("TestStorvsp shouldn't have reached here!");
    }

    async fn negotiate(&mut self) -> Result<(), StorvscError> {
        let mut has_begin_initialization = false;
        let mut has_query_protocol_version = false;
        let mut has_query_properties = false;
        let mut has_end_initialization = false;
        while !has_end_initialization {
            tracing::trace!("Waiting for next initialization packet");
            let (mut reader, mut writer) = self.queue.split();
            let packet = reader
                .read()
                .await
                .map_err(|err| StorvscError(StorvscErrorInner::Queue(err)))
                .unwrap();
            let stor_packet = parse_storvsp_packet(&packet, &mut self.full_request_pool)
                .map_err(|err| StorvscError(StorvscErrorInner::PacketError(err)))
                .unwrap();

            match stor_packet.data {
                StorvspPacketData::BeginInitialization => {
                    tracing::debug!("Received BeginInitialization");

                    // Ensure that subsequent calls to `send_completion` won't
                    // fail due to lack of ring space, to avoid keeping (and saving/restoring) interim states.
                    poll_fn(|cx| self.inner.poll_for_ring_space(cx, &mut writer)).await?;

                    if !has_begin_initialization
                        && !has_query_protocol_version
                        && !has_query_properties
                        && !has_end_initialization
                    {
                        has_begin_initialization = true;
                        self.inner.send_completion(
                            &mut writer,
                            &stor_packet,
                            storvsp_protocol::NtStatus::SUCCESS,
                            &(),
                        )?;
                    } else {
                        tracing::warn!(data = ?stor_packet.data, "Unexpected initialization packet order");
                        self.inner.send_completion(
                            &mut writer,
                            &stor_packet,
                            storvsp_protocol::NtStatus::INVALID_DEVICE_STATE,
                            &(),
                        )?;
                    }
                    Ok(())
                }
                StorvspPacketData::QueryProtocolVersion(major_minor) => {
                    tracing::debug!(major_minor = major_minor, "Received QueryProtocolVersion");
                    if has_begin_initialization
                        && !has_query_protocol_version
                        && !has_query_properties
                        && !has_end_initialization
                    {
                        has_query_protocol_version = true;

                        if let Ok(version) = Version::parse(major_minor) {
                            self.inner.send_completion(
                                &mut writer,
                                &stor_packet,
                                storvsp_protocol::NtStatus::SUCCESS,
                                &storvsp_protocol::ProtocolVersion {
                                    major_minor,
                                    reserved: 0,
                                },
                            )?;
                            self.inner.request_size = version.max_request_size();
                        } else {
                            self.inner.send_completion(
                                &mut writer,
                                &stor_packet,
                                storvsp_protocol::NtStatus::REVISION_MISMATCH,
                                &storvsp_protocol::ProtocolVersion {
                                    major_minor,
                                    reserved: 0,
                                },
                            )?;
                        }
                    } else {
                        tracing::warn!(data = ?stor_packet.data, "Unexpected initialization packet order");
                        self.inner.send_completion(
                            &mut writer,
                            &stor_packet,
                            storvsp_protocol::NtStatus::INVALID_DEVICE_STATE,
                            &(),
                        )?;
                    }
                    Ok(())
                }
                StorvspPacketData::QueryProperties => {
                    tracing::debug!("Received QueryProperties");
                    if has_begin_initialization
                        && has_query_protocol_version
                        && !has_query_properties
                        && !has_end_initialization
                    {
                        has_query_properties = true;
                        self.inner.send_completion(
                            &mut writer,
                            &stor_packet,
                            storvsp_protocol::NtStatus::SUCCESS,
                            &storvsp_protocol::ChannelProperties {
                                max_transfer_bytes: 0x40000, // 256KB
                                flags: storvsp_protocol::STORAGE_CHANNEL_SUPPORTS_MULTI_CHANNEL,
                                maximum_sub_channel_count: 16,
                                reserved: 0,
                                reserved2: 0,
                                reserved3: [0, 0],
                            },
                        )?;
                    } else {
                        tracing::warn!(data = ?stor_packet.data, "Unexpected initialization packet order");
                        self.inner.send_completion(
                            &mut writer,
                            &stor_packet,
                            storvsp_protocol::NtStatus::INVALID_DEVICE_STATE,
                            &(),
                        )?;
                    }
                    Ok(())
                }
                StorvspPacketData::CreateSubChannels(sub_channel_count) => {
                    tracing::debug!(
                        sub_channel_count = sub_channel_count,
                        "Received CreateSubChannels"
                    );
                    self.subchannel_count = sub_channel_count;
                    self.inner.send_completion(
                        &mut writer,
                        &stor_packet,
                        storvsp_protocol::NtStatus::SUCCESS,
                        &(),
                    )?;
                    Ok(())
                }
                StorvspPacketData::EndInitialization => {
                    tracing::debug!("Received EndInitialization");
                    if has_begin_initialization
                        && has_query_protocol_version
                        && has_query_properties
                        && !has_end_initialization
                    {
                        has_end_initialization = true;
                        self.inner.send_completion(
                            &mut writer,
                            &stor_packet,
                            storvsp_protocol::NtStatus::SUCCESS,
                            &(),
                        )?;
                    } else {
                        tracing::warn!(data = ?stor_packet.data, "Unexpected initialization packet order");
                        self.inner.send_completion(
                            &mut writer,
                            &stor_packet,
                            storvsp_protocol::NtStatus::INVALID_DEVICE_STATE,
                            &(),
                        )?;
                    }
                    Ok(())
                }
                _ => {
                    tracing::warn!(data = ?stor_packet.data, "Unexpected packet received during initialization");
                    self.inner.send_completion(
                        &mut writer,
                        &stor_packet,
                        storvsp_protocol::NtStatus::INVALID_DEVICE_STATE,
                        &(),
                    )?;
                    Ok(())
                }
            }?;
        }

        tracing::info!(
            version = self.version.major_minor,
            subchannel_count = self.subchannel_count,
            "storvsp negoiated"
        );

        Ok(())
    }

    async fn process_packets(&mut self) -> Result<(), StorvscError> {
        loop {
            enum Event<'a, M: RingMem> {
                NewCommandRequestReceived(Result<TestStorvspCommandRequest, RecvError>),
                VmbusPacketReceived(Result<PacketRef<'a, M>, queue::Error>),
            }
            let (mut reader, mut writer) = self.queue.split();
            match (
                self.command_request_receiver
                    .recv()
                    .map(Event::NewCommandRequestReceived),
                reader.read().map(Event::VmbusPacketReceived),
            )
                .race()
                .await
            {
                Event::NewCommandRequestReceived(result) => match result {
                    Ok(request) => self.inner.send_vmbus_packet(
                        &mut writer.batched(),
                        if request.requires_completion {
                            OutgoingPacketType::InBandWithCompletion
                        } else {
                            OutgoingPacketType::InBandNoCompletion
                        },
                        request.payload_size,
                        request.transaction_id,
                        request.packet.operation,
                        request.packet.status,
                        request.payload.as_slice(),
                    ),
                    Err(_err) => Err(StorvscError(StorvscErrorInner::RequestError)),
                },
                Event::VmbusPacketReceived(result) => match result {
                    Ok(packet) => {
                        let stor_packet =
                            parse_storvsp_packet(&packet, &mut self.full_request_pool)
                                .map_err(|err| StorvscError(StorvscErrorInner::PacketError(err)))?;
                        tracing::info!("storvsp received request packet");

                        match stor_packet.data.clone() {
                            StorvspPacketData::ExecuteScsi(_request) => {
                                tracing::info!("storvsp responding to EXECUTE_SRB");
                                self.inner.send_completion(
                                    &mut writer,
                                    &stor_packet,
                                    storvsp_protocol::NtStatus::SUCCESS,
                                    &(),
                                )?;
                            }
                            _ => {
                                tracing::info!("storvsp received unexpected request packet type");
                                self.inner.send_completion(
                                    &mut writer,
                                    &stor_packet,
                                    storvsp_protocol::NtStatus::INVALID_DEVICE_STATE,
                                    &(),
                                )?;
                            }
                        }
                        Ok(())
                    }
                    Err(err) => Err(StorvscError(StorvscErrorInner::Queue(err))),
                },
            }?;
        }
    }
}

impl TestStorvspInner {
    fn send_completion<M: RingMem, P: IntoBytes + Immutable + KnownLayout>(
        &mut self,
        writer: &mut queue::WriteHalf<'_, M>,
        packet: &StorvspPacket,
        status: storvsp_protocol::NtStatus,
        payload: &P,
    ) -> Result<(), StorvscError> {
        self.send_vmbus_packet(
            &mut writer.batched(),
            OutgoingPacketType::Completion,
            packet.request_size,
            packet.transaction_id,
            storvsp_protocol::Operation::COMPLETE_IO,
            status,
            payload.as_bytes(),
        )
    }

    fn send_vmbus_packet<M: RingMem>(
        &mut self,
        writer: &mut queue::WriteBatch<'_, M>,
        packet_type: OutgoingPacketType<'_>,
        _request_size: usize, // Unused, but kept for compatibility with similar APIs
        transaction_id: u64,
        operation: storvsp_protocol::Operation,
        status: storvsp_protocol::NtStatus,
        payload: &[u8],
    ) -> Result<(), StorvscError> {
        let header = storvsp_protocol::Packet {
            operation,
            flags: 0,
            status,
        };

        writer
            .try_write(&OutgoingPacket {
                transaction_id,
                packet_type,
                payload: &[header.as_bytes(), payload],
            })
            .map_err(|err| match err {
                queue::TryWriteError::Full(_) => StorvscError(StorvscErrorInner::NotEnoughSpace),
                queue::TryWriteError::Queue(err) => StorvscError(StorvscErrorInner::Queue(err)),
            })
    }

    /// Polls for enough ring space in the outgoing ring to send a packet.
    ///
    /// This is used to ensure there is enough space in the ring before
    /// committing to sending a packet. This avoids the need to save pending
    /// packets on the side if queue processing is interrupted while the ring is
    /// full.
    fn poll_for_ring_space<M: RingMem>(
        &mut self,
        cx: &mut Context<'_>,
        writer: &mut queue::WriteHalf<'_, M>,
    ) -> Poll<Result<(), StorvscError>> {
        writer
            .poll_ready(cx, MAX_VMBUS_PACKET_SIZE)
            .map_err(|err| StorvscError(StorvscErrorInner::Queue(err)))
    }
}
