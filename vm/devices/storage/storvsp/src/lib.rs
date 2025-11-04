// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

#[cfg(feature = "ioperf")]
pub mod ioperf;

#[cfg(feature = "test")]
pub mod test_helpers;

#[cfg(not(feature = "test"))]
mod test_helpers;

pub mod resolver;
mod save_restore;

use crate::ring::gparange::MultiPagedRangeBuf;
use anyhow::Context as _;
use async_trait::async_trait;
use fast_select::FastSelect;
use futures::FutureExt;
use futures::StreamExt;
use futures::select_biased;
use guestmem::AccessError;
use guestmem::GuestMemory;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use guestmem::ranges::PagedRange;
use guid::Guid;
use inspect::Inspect;
use inspect::InspectMut;
use inspect_counters::Counter;
use inspect_counters::Histogram;
use oversized_box::OversizedBox;
use parking_lot::Mutex;
use parking_lot::RwLock;
use ring::OutgoingPacketType;
use scsi::AdditionalSenseCode;
use scsi::ScsiOp;
use scsi::ScsiStatus;
use scsi::srb::SrbStatus;
use scsi::srb::SrbStatusAndFlags;
use scsi_buffers::RequestBuffers;
use scsi_core::AsyncScsiDisk;
use scsi_core::Request;
use scsi_core::ScsiResult;
use scsi_defs as scsi;
use scsidisk::illegal_request_sense;
use slab::Slab;
use std::collections::hash_map::Entry;
use std::collections::hash_map::HashMap;
use std::fmt::Debug;
use std::future::Future;
use std::future::poll_fn;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering::Relaxed;
use std::task::Context;
use std::task::Poll;
use storvsp_resources::ScsiPath;
use task_control::AsyncRun;
use task_control::InspectTask;
use task_control::StopTask;
use task_control::TaskControl;
use thiserror::Error;
use tracing_helpers::ErrorValueExt;
use unicycle::FuturesUnordered;
use vmbus_async::queue;
use vmbus_async::queue::ExternalDataError;
use vmbus_async::queue::IncomingPacket;
use vmbus_async::queue::OutgoingPacket;
use vmbus_async::queue::Queue;
use vmbus_channel::RawAsyncChannel;
use vmbus_channel::bus::ChannelType;
use vmbus_channel::bus::OfferParams;
use vmbus_channel::bus::OpenRequest;
use vmbus_channel::channel::ChannelControl;
use vmbus_channel::channel::ChannelOpenError;
use vmbus_channel::channel::DeviceResources;
use vmbus_channel::channel::RestoreControl;
use vmbus_channel::channel::SaveRestoreVmbusDevice;
use vmbus_channel::channel::VmbusDevice;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_channel::gpadl_ring::gpadl_channel;
use vmbus_core::protocol::UserDefinedData;
use vmbus_ring as ring;
use vmbus_ring::RingMem;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// The IO queue depth at which the controller switches from guest-signal-driven
/// to poll-mode operation. This optimization reduces the guest exit rate by
/// relying on (typically-interrupt-driven) IO completions to drive polling for
/// new IO requests.
const DEFAULT_POLL_MODE_QUEUE_DEPTH: u32 = 1;

pub struct StorageDevice {
    instance_id: Guid,
    ide_path: Option<ScsiPath>,
    workers: Vec<WorkerAndDriver>,
    controller: Arc<ScsiControllerState>,
    resources: DeviceResources,
    driver_source: VmTaskDriverSource,
    max_sub_channel_count: u16,
    protocol: Arc<Protocol>,
    io_queue_depth: u32,
}

#[derive(Inspect)]
struct WorkerAndDriver {
    #[inspect(flatten)]
    worker: TaskControl<WorkerState, Worker>,
    driver: VmTaskDriver,
}

struct WorkerState;

impl InspectMut for StorageDevice {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        let mut resp = req.respond();

        let disks = self.controller.disks.read();
        for (path, controller_disk) in disks.iter() {
            resp.child(&format!("disks/{}", path), |req| {
                controller_disk.disk.inspect(req);
            });
        }

        resp.fields(
            "channels",
            self.workers
                .iter()
                .filter(|task| task.worker.has_state())
                .enumerate(),
        )
        .field(
            "poll_mode_queue_depth",
            inspect::AtomicMut(&self.controller.poll_mode_queue_depth),
        );
    }
}

struct Worker<T: RingMem = GpadlRingMem> {
    inner: WorkerInner,
    rescan_notification: futures::channel::mpsc::Receiver<()>,
    fast_select: FastSelect,
    queue: Queue<T>,
}

struct Protocol {
    state: RwLock<ProtocolState>,
    /// Signaled when `state` transitions to `ProtocolState::Ready`.
    ready: event_listener::Event,
}

struct WorkerInner {
    protocol: Arc<Protocol>,
    request_size: usize,
    controller: Arc<ScsiControllerState>,
    channel_index: u16,
    scsi_queue: Arc<ScsiCommandQueue>,
    scsi_requests: FuturesUnordered<ScsiRequest>,
    scsi_requests_states: Slab<ScsiRequestState>,
    full_request_pool: Vec<Arc<ScsiRequestAndRange>>,
    future_pool: Vec<OversizedBox<(), ScsiOpStorage>>,
    channel_control: ChannelControl,
    max_io_queue_depth: usize,
    stats: WorkerStats,
}

#[derive(Debug, Default, Inspect)]
struct WorkerStats {
    ios_submitted: Counter,
    ios_completed: Counter,
    wakes: Counter,
    wakes_spurious: Counter,
    per_wake_submissions: Histogram<10>,
    per_wake_completions: Histogram<10>,
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

#[derive(Copy, Clone)]
enum ProtocolState {
    Init(InitState),
    Ready {
        version: Version,
        subchannel_count: u16,
    },
}

#[derive(Copy, Clone, Debug)]
enum InitState {
    Begin,
    QueryVersion,
    QueryProperties {
        version: Version,
    },
    EndInitialization {
        version: Version,
        subchannel_count: Option<u16>,
    },
}

/// The internal SCSI operation future type.
///
/// This is a boxed future of a large pre-determined size. The box is reused
/// after a SCSI request completes to avoid allocations in the hot path.
///
/// An Option type is used so that the future can be efficiently dropped (via
/// `Pin::set(x, None)`) before it is stashed away for reuse.
type ScsiOpStorage = [u64; SCSI_REQUEST_STACK_SIZE / 8];
type ScsiOpFuture = Pin<OversizedBox<dyn Future<Output = ScsiResult> + Send, ScsiOpStorage>>;

/// The amount of space reserved for a ScsiOpFuture.
///
/// This was chosen by running `cargo test -p storvsp -- --no-capture` and looking at the required
/// size that was given in the failure message
const SCSI_REQUEST_STACK_SIZE: usize = scsi_core::ASYNC_SCSI_DISK_STACK_SIZE + 272;

struct ScsiRequest {
    request_id: usize,
    future: Option<ScsiOpFuture>,
}

impl ScsiRequest {
    fn new(
        request_id: usize,
        future: OversizedBox<dyn Future<Output = ScsiResult> + Send, ScsiOpStorage>,
    ) -> Self {
        Self {
            request_id,
            future: Some(future.into()),
        }
    }
}

impl Future for ScsiRequest {
    type Output = (usize, ScsiResult, OversizedBox<(), ScsiOpStorage>);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let future = this.future.as_mut().unwrap().as_mut();
        let result = std::task::ready!(future.poll(cx));
        // Return the future so that its storage can be reused.
        let future = this.future.take().unwrap();
        Poll::Ready((this.request_id, result, OversizedBox::empty_pinned(future)))
    }
}

#[derive(Debug, Error)]
enum WorkerError {
    #[error("packet error")]
    PacketError(#[source] PacketError),
    #[error("queue error")]
    Queue(#[source] queue::Error),
    #[error("queue should have enough space but no longer does")]
    NotEnoughSpace,
}

#[derive(Debug, Error)]
enum PacketError {
    #[error("Not transactional")]
    NotTransactional,
    #[error("Unrecognized operation {0:?}")]
    UnrecognizedOperation(storvsp_protocol::Operation),
    #[error("Invalid packet type")]
    InvalidPacketType,
    #[error("Invalid data transfer length")]
    InvalidDataTransferLength,
    #[error("Access error: {0}")]
    Access(#[source] AccessError),
    #[error("Range error")]
    Range(#[source] ExternalDataError),
}

#[derive(Debug, Default, Clone)]
struct Range {
    len: usize,
    is_write: bool,
}

impl Range {
    fn new(buf: &MultiPagedRangeBuf, request: &storvsp_protocol::ScsiRequest) -> Option<Self> {
        let len = request.data_transfer_length as usize;
        let is_write = request.data_in != 0;
        // Ensure there is exactly one range and it's large enough, or there are
        // zero ranges and there is no associated SCSI buffer.
        if buf.range_count() > 1 || (len > 0 && buf.first()?.len() < len) {
            return None;
        }
        Some(Self { len, is_write })
    }

    fn buffer<'a>(
        &'a self,
        buf: &'a MultiPagedRangeBuf,
        guest_memory: &'a GuestMemory,
    ) -> RequestBuffers<'a> {
        let mut range = buf.first().unwrap_or_else(PagedRange::empty);
        range.truncate(self.len);
        RequestBuffers::new(guest_memory, range, self.is_write)
    }
}

#[derive(Debug)]
struct Packet {
    data: PacketData,
    transaction_id: u64,
    request_size: usize,
}

#[derive(Debug)]
enum PacketData {
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

#[derive(Debug)]
pub struct RangeError;

fn parse_packet<T: RingMem>(
    packet: &IncomingPacket<'_, T>,
    pool: &mut Vec<Arc<ScsiRequestAndRange>>,
) -> Result<Packet, PacketError> {
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
        storvsp_protocol::Operation::BEGIN_INITIALIZATION => PacketData::BeginInitialization,
        storvsp_protocol::Operation::END_INITIALIZATION => PacketData::EndInitialization,
        storvsp_protocol::Operation::QUERY_PROTOCOL_VERSION => {
            let mut version = storvsp_protocol::ProtocolVersion::new_zeroed();
            reader
                .read(version.as_mut_bytes())
                .map_err(PacketError::Access)?;
            PacketData::QueryProtocolVersion(version.major_minor)
        }
        storvsp_protocol::Operation::QUERY_PROPERTIES => PacketData::QueryProperties,
        storvsp_protocol::Operation::EXECUTE_SRB => {
            let mut full_request = pool.pop().unwrap_or_else(|| {
                Arc::new(ScsiRequestAndRange {
                    external_data: Range::default(),
                    external_data_buf: MultiPagedRangeBuf::new(),
                    request: storvsp_protocol::ScsiRequest::new_zeroed(),
                    request_size,
                })
            });

            {
                let full_request = Arc::get_mut(&mut full_request).unwrap();
                let request_buf = &mut full_request.request.as_mut_bytes()[..request_size];
                reader.read(request_buf).map_err(PacketError::Access)?;

                full_request.external_data_buf.clear();
                packet
                    .read_external_ranges(&mut full_request.external_data_buf)
                    .map_err(PacketError::Range)?;

                full_request.external_data =
                    Range::new(&full_request.external_data_buf, &full_request.request)
                        .ok_or(PacketError::InvalidDataTransferLength)?;
            }

            PacketData::ExecuteScsi(full_request)
        }
        storvsp_protocol::Operation::RESET_LUN => PacketData::ResetLun,
        storvsp_protocol::Operation::RESET_ADAPTER => PacketData::ResetAdapter,
        storvsp_protocol::Operation::RESET_BUS => PacketData::ResetBus,
        storvsp_protocol::Operation::CREATE_SUB_CHANNELS => {
            let mut sub_channel_count: u16 = 0;
            reader
                .read(sub_channel_count.as_mut_bytes())
                .map_err(PacketError::Access)?;
            PacketData::CreateSubChannels(sub_channel_count)
        }
        _ => return Err(PacketError::UnrecognizedOperation(header.operation)),
    };

    if let PacketData::ExecuteScsi(_) = data {
        tracing::trace!(transaction_id, ?data, "parse_packet");
    } else {
        tracing::debug!(transaction_id, ?data, "parse_packet");
    }

    Ok(Packet {
        data,
        request_size,
        transaction_id,
    })
}

impl WorkerInner {
    fn send_vmbus_packet<M: RingMem>(
        &mut self,
        writer: &mut queue::WriteBatch<'_, M>,
        packet_type: OutgoingPacketType<'_>,
        request_size: usize,
        transaction_id: u64,
        operation: storvsp_protocol::Operation,
        status: storvsp_protocol::NtStatus,
        payload: &[u8],
    ) -> Result<(), WorkerError> {
        let header = storvsp_protocol::Packet {
            operation,
            flags: 0,
            status,
        };

        let packet_size = size_of_val(&header) + request_size;

        // Zero pad or truncate the payload to the queue's packet size. This is
        // necessary because Windows guests check that each packet's size is
        // exactly the largest possible packet size for the negotiated protocol
        // version.
        let len = size_of_val(&header) + size_of_val(payload);
        let padding = [0; storvsp_protocol::SCSI_REQUEST_LEN_MAX];
        let (payload_bytes, padding_bytes) = if len > packet_size {
            (&payload[..packet_size - size_of_val(&header)], &[][..])
        } else {
            (payload, &padding[..packet_size - len])
        };
        assert_eq!(
            size_of_val(&header) + payload_bytes.len() + padding_bytes.len(),
            packet_size
        );
        writer
            .try_write(&OutgoingPacket {
                transaction_id,
                packet_type,
                payload: &[header.as_bytes(), payload_bytes, padding_bytes],
            })
            .map_err(|err| match err {
                queue::TryWriteError::Full(_) => WorkerError::NotEnoughSpace,
                queue::TryWriteError::Queue(err) => WorkerError::Queue(err),
            })
    }

    fn send_packet<M: RingMem, P: IntoBytes + Immutable + KnownLayout>(
        &mut self,
        writer: &mut queue::WriteHalf<'_, M>,
        operation: storvsp_protocol::Operation,
        status: storvsp_protocol::NtStatus,
        payload: &P,
    ) -> Result<(), WorkerError> {
        self.send_vmbus_packet(
            &mut writer.batched(),
            OutgoingPacketType::InBandNoCompletion,
            self.request_size,
            0,
            operation,
            status,
            payload.as_bytes(),
        )
    }

    fn send_completion<M: RingMem, P: IntoBytes + Immutable + KnownLayout>(
        &mut self,
        writer: &mut queue::WriteHalf<'_, M>,
        packet: &Packet,
        status: storvsp_protocol::NtStatus,
        payload: &P,
    ) -> Result<(), WorkerError> {
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
}

struct ScsiCommandQueue {
    controller: Arc<ScsiControllerState>,
    mem: GuestMemory,
    force_path_id: Option<u8>,
}

impl ScsiCommandQueue {
    async fn execute_scsi(&self, full_request: &ScsiRequestAndRange) -> ScsiResult {
        let request = &full_request.request;
        let op = ScsiOp(request.payload[0]);
        let external_data = full_request
            .external_data
            .buffer(&full_request.external_data_buf, &self.mem);

        tracing::trace!(
            path_id = request.path_id,
            target_id = request.target_id,
            lun = request.lun,
            op = ?op,
            "execute_scsi start...",
        );

        let path_id = self.force_path_id.unwrap_or(request.path_id);

        let controller_disk = self
            .controller
            .disks
            .read()
            .get(&ScsiPath {
                path: path_id,
                target: request.target_id,
                lun: request.lun,
            })
            .cloned();

        let result = match op {
            ScsiOp::REPORT_LUNS => {
                const HEADER_SIZE: usize = size_of::<scsi::LunList>();
                let mut luns: Vec<u8> = self
                    .controller
                    .disks
                    .read()
                    .iter()
                    .flat_map(|(path, _)| {
                        // Use the original path ID and not the forced one to
                        // match Hyper-V storvsp behavior.
                        if request.path_id == path.path && request.target_id == path.target {
                            Some(path.lun)
                        } else {
                            None
                        }
                    })
                    .collect();
                luns.sort_unstable();
                let mut data: Vec<u64> = vec![0; luns.len() + 1];
                let header = scsi::LunList {
                    length: (luns.len() as u32 * 8).into(),
                    reserved: [0; 4],
                };
                data.as_mut_bytes()[..HEADER_SIZE].copy_from_slice(header.as_bytes());
                for (i, lun) in luns.iter().enumerate() {
                    data[i + 1].as_mut_bytes()[..2].copy_from_slice(&(*lun as u16).to_be_bytes());
                }
                if external_data.len() >= HEADER_SIZE {
                    let tx = std::cmp::min(external_data.len(), data.as_bytes().len());
                    external_data.writer().write(&data.as_bytes()[..tx]).map_or(
                        ScsiResult {
                            scsi_status: ScsiStatus::CHECK_CONDITION,
                            srb_status: SrbStatus::INVALID_REQUEST,
                            tx: 0,
                            sense_data: Some(illegal_request_sense(
                                AdditionalSenseCode::INVALID_CDB,
                            )),
                        },
                        |_| ScsiResult {
                            scsi_status: ScsiStatus::GOOD,
                            srb_status: SrbStatus::SUCCESS,
                            tx,
                            sense_data: None,
                        },
                    )
                } else {
                    ScsiResult {
                        scsi_status: ScsiStatus::GOOD,
                        srb_status: SrbStatus::SUCCESS,
                        tx: 0,
                        sense_data: None,
                    }
                }
            }
            _ if controller_disk.is_some() => {
                let mut cdb = [0; 16];
                cdb.copy_from_slice(&request.payload[0..storvsp_protocol::CDB16GENERIC_LENGTH]);
                controller_disk
                    .unwrap()
                    .disk
                    .execute_scsi(
                        &external_data,
                        &Request {
                            cdb,
                            srb_flags: request.srb_flags,
                        },
                    )
                    .await
            }
            ScsiOp::INQUIRY => {
                let cdb = scsi::CdbInquiry::ref_from_prefix(&request.payload)
                    .unwrap()
                    .0; // TODO: zerocopy: ref-from-prefix: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
                if external_data.len() < cdb.allocation_length.get() as usize
                    || request.data_in != storvsp_protocol::SCSI_IOCTL_DATA_IN
                    || (cdb.allocation_length.get() as usize) < size_of::<scsi::InquiryDataHeader>()
                {
                    ScsiResult {
                        scsi_status: ScsiStatus::CHECK_CONDITION,
                        srb_status: SrbStatus::INVALID_REQUEST,
                        tx: 0,
                        sense_data: Some(illegal_request_sense(AdditionalSenseCode::INVALID_CDB)),
                    }
                } else {
                    let enable_vpd = cdb.flags.vpd();
                    if enable_vpd || cdb.page_code != 0 {
                        // cannot support VPD inquiry for non-existing device (lun).
                        ScsiResult {
                            scsi_status: ScsiStatus::CHECK_CONDITION,
                            srb_status: SrbStatus::INVALID_REQUEST,
                            tx: 0,
                            sense_data: Some(illegal_request_sense(
                                AdditionalSenseCode::INVALID_CDB,
                            )),
                        }
                    } else {
                        const LOGICAL_UNIT_NOT_PRESENT_DEVICE: u8 = 0x7F;
                        let mut data = scsidisk::INQUIRY_DATA_TEMPLATE;
                        data.header.device_type = LOGICAL_UNIT_NOT_PRESENT_DEVICE;

                        if request.lun != 0 {
                            // Below fields are only set for lun0 inquiry so zero out here.
                            data.vendor_id = [0; 8];
                            data.product_id = [0; 16];
                            data.product_revision_level = [0; 4];
                        }

                        let datab = data.as_bytes();
                        let tx = std::cmp::min(
                            cdb.allocation_length.get() as usize,
                            size_of::<scsi::InquiryData>(),
                        );
                        external_data.writer().write(&datab[..tx]).map_or(
                            ScsiResult {
                                scsi_status: ScsiStatus::CHECK_CONDITION,
                                srb_status: SrbStatus::INVALID_REQUEST,
                                tx: 0,
                                sense_data: Some(illegal_request_sense(
                                    AdditionalSenseCode::INVALID_CDB,
                                )),
                            },
                            |_| ScsiResult {
                                scsi_status: ScsiStatus::GOOD,
                                srb_status: SrbStatus::SUCCESS,
                                tx,
                                sense_data: None,
                            },
                        )
                    }
                }
            }
            _ => ScsiResult {
                scsi_status: ScsiStatus::CHECK_CONDITION,
                srb_status: SrbStatus::INVALID_LUN,
                tx: 0,
                sense_data: None,
            },
        };

        tracing::trace!(
            path_id = request.path_id,
            target_id = request.target_id,
            lun = request.lun,
            op = ?op,
            result = ?result,
            "execute_scsi completed.",
        );
        result
    }
}

impl<T: RingMem + 'static> Worker<T> {
    fn new(
        controller: Arc<ScsiControllerState>,
        channel: RawAsyncChannel<T>,
        channel_index: u16,
        mem: GuestMemory,
        channel_control: ChannelControl,
        io_queue_depth: u32,
        protocol: Arc<Protocol>,
        force_path_id: Option<u8>,
    ) -> anyhow::Result<Self> {
        let queue = Queue::new(channel)?;
        #[expect(clippy::disallowed_methods)] // TODO
        let (source, target) = futures::channel::mpsc::channel(1);
        controller.add_rescan_notification_source(source);

        let max_io_queue_depth = io_queue_depth.max(1) as usize;
        Ok(Self {
            inner: WorkerInner {
                protocol,
                request_size: storvsp_protocol::SCSI_REQUEST_LEN_V1,
                controller: controller.clone(),
                channel_index,
                scsi_queue: Arc::new(ScsiCommandQueue {
                    controller,
                    mem,
                    force_path_id,
                }),
                scsi_requests: FuturesUnordered::new(),
                scsi_requests_states: Slab::with_capacity(max_io_queue_depth),
                channel_control,
                max_io_queue_depth,
                future_pool: Vec::new(),
                full_request_pool: Vec::new(),
                stats: Default::default(),
            },
            queue,
            rescan_notification: target,
            fast_select: FastSelect::new(),
        })
    }

    async fn wait_for_scsi_requests_complete(&mut self) {
        tracing::debug!(
            channel_index = self.inner.channel_index,
            "wait for IOs completed..."
        );
        while let Some((id, _, _)) = self.inner.scsi_requests.next().await {
            self.inner.scsi_requests_states.remove(id);
        }
    }
}

impl InspectTask<Worker> for WorkerState {
    fn inspect(&self, req: inspect::Request<'_>, worker: Option<&Worker>) {
        if let Some(worker) = worker {
            let mut resp = req.respond();
            if worker.inner.channel_index == 0 {
                let (state, version, subchannel_count) = match *worker.inner.protocol.state.read() {
                    ProtocolState::Init(state) => match state {
                        InitState::Begin => ("begin_init", None, None),
                        InitState::QueryVersion => ("query_version", None, None),
                        InitState::QueryProperties { version } => {
                            ("query_properties", Some(version), None)
                        }
                        InitState::EndInitialization {
                            version,
                            subchannel_count,
                        } => ("end_init", Some(version), subchannel_count),
                    },
                    ProtocolState::Ready {
                        version,
                        subchannel_count,
                    } => ("ready", Some(version), Some(subchannel_count)),
                };
                resp.field("state", state)
                    .field("version", version)
                    .field("subchannel_count", subchannel_count);
            }
            resp.field("pending_packets", worker.inner.scsi_requests_states.len())
                .fields("io", worker.inner.scsi_requests_states.iter())
                .field("stats", &worker.inner.stats)
                .field("ring", &worker.queue)
                .field("max_io_queue_depth", worker.inner.max_io_queue_depth);
        }
    }
}

impl<T: 'static + Send + Sync + RingMem> AsyncRun<Worker<T>> for WorkerState {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        worker: &mut Worker<T>,
    ) -> Result<(), task_control::Cancelled> {
        let fut = async {
            if worker.inner.channel_index == 0 {
                worker.process_primary().await
            } else {
                // Wait for initialization to end before processing any
                // subchannel packets.
                let protocol_version = loop {
                    let listener = worker.inner.protocol.ready.listen();
                    if let ProtocolState::Ready { version, .. } =
                        *worker.inner.protocol.state.read()
                    {
                        break version;
                    }
                    tracing::debug!("subchannel waiting for initialization to end");
                    listener.await
                };
                worker
                    .inner
                    .process_ready(&mut worker.queue, protocol_version)
                    .await
            }
        };

        match stop.until_stopped(fut).await? {
            Ok(_) => {}
            Err(e) => tracing::error!(error = e.as_error(), "process_packets error"),
        }
        Ok(())
    }
}

impl WorkerInner {
    /// Awaits the next incoming packet, without checking for any other events (device add/remove notifications or available completions).
    /// Increments the count of outstanding packets when returning `Ok(Packet)`.
    async fn next_packet<'a, M: RingMem>(
        &mut self,
        reader: &'a mut queue::ReadHalf<'a, M>,
    ) -> Result<Packet, WorkerError> {
        let packet = reader.read().await.map_err(WorkerError::Queue)?;
        let stor_packet =
            parse_packet(&packet, &mut self.full_request_pool).map_err(WorkerError::PacketError)?;
        Ok(stor_packet)
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
    ) -> Poll<Result<(), WorkerError>> {
        writer
            .poll_ready(cx, MAX_VMBUS_PACKET_SIZE)
            .map_err(WorkerError::Queue)
    }
}

const MAX_VMBUS_PACKET_SIZE: usize = ring::PacketSize::in_band(
    size_of::<storvsp_protocol::Packet>() + storvsp_protocol::SCSI_REQUEST_LEN_MAX,
);

impl<T: RingMem> Worker<T> {
    /// Processes the protocol state machine.
    async fn process_primary(&mut self) -> Result<(), WorkerError> {
        loop {
            let current_state = *self.inner.protocol.state.read();
            match current_state {
                ProtocolState::Ready { version, .. } => {
                    break loop {
                        select_biased! {
                            r = self.inner.process_ready(&mut self.queue, version).fuse() => break r,
                            _ = self.fast_select.select((self.rescan_notification.select_next_some(),)).fuse() => {
                                if version >= Version::Win7
                                {
                                    self.inner.send_packet(&mut self.queue.split().1, storvsp_protocol::Operation::ENUMERATE_BUS, storvsp_protocol::NtStatus::SUCCESS, &())?;
                                }
                            }
                        }
                    };
                }
                ProtocolState::Init(state) => {
                    let (mut reader, mut writer) = self.queue.split();

                    // Ensure that subsequent calls to `send_completion` won't
                    // fail due to lack of ring space, to avoid keeping (and saving/restoring) interim states.
                    poll_fn(|cx| self.inner.poll_for_ring_space(cx, &mut writer)).await?;

                    tracing::debug!(?state, "process_primary");
                    match state {
                        InitState::Begin => {
                            let packet = self.inner.next_packet(&mut reader).await?;
                            if let PacketData::BeginInitialization = packet.data {
                                self.inner.send_completion(
                                    &mut writer,
                                    &packet,
                                    storvsp_protocol::NtStatus::SUCCESS,
                                    &(),
                                )?;
                                *self.inner.protocol.state.write() =
                                    ProtocolState::Init(InitState::QueryVersion);
                            } else {
                                tracelimit::warn_ratelimited!(?state, data = ?packet.data, "unexpected packet order");
                                self.inner.send_completion(
                                    &mut writer,
                                    &packet,
                                    storvsp_protocol::NtStatus::INVALID_DEVICE_STATE,
                                    &(),
                                )?;
                            }
                        }
                        InitState::QueryVersion => {
                            let packet = self.inner.next_packet(&mut reader).await?;
                            if let PacketData::QueryProtocolVersion(major_minor) = packet.data {
                                if let Ok(version) = Version::parse(major_minor) {
                                    self.inner.send_completion(
                                        &mut writer,
                                        &packet,
                                        storvsp_protocol::NtStatus::SUCCESS,
                                        &storvsp_protocol::ProtocolVersion {
                                            major_minor,
                                            reserved: 0,
                                        },
                                    )?;
                                    self.inner.request_size = version.max_request_size();
                                    *self.inner.protocol.state.write() =
                                        ProtocolState::Init(InitState::QueryProperties { version });

                                    tracelimit::info_ratelimited!(
                                        ?version,
                                        "scsi version negotiated"
                                    );
                                } else {
                                    self.inner.send_completion(
                                        &mut writer,
                                        &packet,
                                        storvsp_protocol::NtStatus::REVISION_MISMATCH,
                                        &storvsp_protocol::ProtocolVersion {
                                            major_minor,
                                            reserved: 0,
                                        },
                                    )?;
                                    *self.inner.protocol.state.write() =
                                        ProtocolState::Init(InitState::QueryVersion);
                                }
                            } else {
                                tracelimit::warn_ratelimited!(?state, data = ?packet.data, "unexpected packet order");
                                self.inner.send_completion(
                                    &mut writer,
                                    &packet,
                                    storvsp_protocol::NtStatus::INVALID_DEVICE_STATE,
                                    &(),
                                )?;
                            }
                        }
                        InitState::QueryProperties { version } => {
                            let packet = self.inner.next_packet(&mut reader).await?;
                            if let PacketData::QueryProperties = packet.data {
                                let multi_channel_supported = version >= Version::Win8;

                                self.inner.send_completion(
                                    &mut writer,
                                    &packet,
                                    storvsp_protocol::NtStatus::SUCCESS,
                                    &storvsp_protocol::ChannelProperties {
                                        max_transfer_bytes: 0x40000, // 256KB
                                        flags: {
                                            if multi_channel_supported {
                                                storvsp_protocol::STORAGE_CHANNEL_SUPPORTS_MULTI_CHANNEL
                                            } else {
                                                0
                                            }
                                        },
                                        maximum_sub_channel_count: if multi_channel_supported {
                                            self.inner.channel_control.max_subchannels()
                                        } else {
                                            0
                                        },
                                        reserved: 0,
                                        reserved2: 0,
                                        reserved3: [0, 0],
                                    },
                                )?;
                                *self.inner.protocol.state.write() =
                                    ProtocolState::Init(InitState::EndInitialization {
                                        version,
                                        subchannel_count: if multi_channel_supported {
                                            None
                                        } else {
                                            Some(0)
                                        },
                                    });
                            } else {
                                tracelimit::warn_ratelimited!(?state, data = ?packet.data, "unexpected packet order");
                                self.inner.send_completion(
                                    &mut writer,
                                    &packet,
                                    storvsp_protocol::NtStatus::INVALID_DEVICE_STATE,
                                    &(),
                                )?;
                            }
                        }
                        InitState::EndInitialization {
                            version,
                            subchannel_count,
                        } => {
                            let packet = self.inner.next_packet(&mut reader).await?;
                            match packet.data {
                                PacketData::CreateSubChannels(sub_channel_count)
                                    if subchannel_count.is_none() =>
                                {
                                    if let Err(err) = self
                                        .inner
                                        .channel_control
                                        .enable_subchannels(sub_channel_count)
                                    {
                                        tracelimit::warn_ratelimited!(
                                            ?err,
                                            "cannot enable subchannels"
                                        );
                                        self.inner.send_completion(
                                            &mut writer,
                                            &packet,
                                            storvsp_protocol::NtStatus::INVALID_PARAMETER,
                                            &(),
                                        )?;
                                    } else {
                                        self.inner.send_completion(
                                            &mut writer,
                                            &packet,
                                            storvsp_protocol::NtStatus::SUCCESS,
                                            &(),
                                        )?;
                                        *self.inner.protocol.state.write() =
                                            ProtocolState::Init(InitState::EndInitialization {
                                                version,
                                                subchannel_count: Some(sub_channel_count),
                                            });
                                    }
                                }
                                PacketData::EndInitialization => {
                                    self.inner.send_completion(
                                        &mut writer,
                                        &packet,
                                        storvsp_protocol::NtStatus::SUCCESS,
                                        &(),
                                    )?;
                                    // Reset the rescan notification event now, before the guest has a
                                    // chance to send any SCSI requests to scan the bus.
                                    self.rescan_notification.try_next().ok();
                                    *self.inner.protocol.state.write() = ProtocolState::Ready {
                                        version,
                                        subchannel_count: subchannel_count.unwrap_or(0),
                                    };
                                    // Wake up subchannels waiting for the
                                    // protocol state to become ready.
                                    self.inner.protocol.ready.notify(usize::MAX);
                                }
                                _ => {
                                    tracelimit::warn_ratelimited!(?state, data = ?packet.data, "unexpected packet order");
                                    self.inner.send_completion(
                                        &mut writer,
                                        &packet,
                                        storvsp_protocol::NtStatus::INVALID_DEVICE_STATE,
                                        &(),
                                    )?;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn convert_srb_status_to_nt_status(srb_status: SrbStatus) -> storvsp_protocol::NtStatus {
    match srb_status {
        SrbStatus::BUSY => storvsp_protocol::NtStatus::DEVICE_BUSY,
        SrbStatus::SUCCESS => storvsp_protocol::NtStatus::SUCCESS,
        SrbStatus::INVALID_LUN
        | SrbStatus::INVALID_TARGET_ID
        | SrbStatus::NO_DEVICE
        | SrbStatus::NO_HBA => storvsp_protocol::NtStatus::DEVICE_DOES_NOT_EXIST,
        SrbStatus::COMMAND_TIMEOUT | SrbStatus::TIMEOUT => storvsp_protocol::NtStatus::IO_TIMEOUT,
        SrbStatus::SELECTION_TIMEOUT => storvsp_protocol::NtStatus::DEVICE_NOT_CONNECTED,
        SrbStatus::BAD_FUNCTION | SrbStatus::BAD_SRB_BLOCK_LENGTH => {
            storvsp_protocol::NtStatus::INVALID_DEVICE_REQUEST
        }
        SrbStatus::DATA_OVERRUN => storvsp_protocol::NtStatus::BUFFER_OVERFLOW,
        SrbStatus::REQUEST_FLUSHED => storvsp_protocol::NtStatus::UNSUCCESSFUL,
        SrbStatus::ABORTED => storvsp_protocol::NtStatus::CANCELLED,
        _ => storvsp_protocol::NtStatus::IO_DEVICE_ERROR,
    }
}

impl WorkerInner {
    /// Processes packets and SCSI completions after protocol negotiation has finished.
    async fn process_ready<M: RingMem>(
        &mut self,
        queue: &mut Queue<M>,
        protocol_version: Version,
    ) -> Result<(), WorkerError> {
        self.request_size = protocol_version.max_request_size();
        poll_fn(|cx| self.poll_process_ready(cx, queue)).await
    }

    /// Processes packets and SCSI completions after protocol negotiation has finished.
    fn poll_process_ready<M: RingMem>(
        &mut self,
        cx: &mut Context<'_>,
        queue: &mut Queue<M>,
    ) -> Poll<Result<(), WorkerError>> {
        self.stats.wakes.increment();

        let (mut reader, mut writer) = queue.split();
        let mut total_completions = 0;
        let mut total_submissions = 0;
        let poll_mode_queue_depth = self.controller.poll_mode_queue_depth.load(Relaxed) as usize;

        loop {
            // Drive IOs forward and collect completions.
            'outer: while !self.scsi_requests_states.is_empty() {
                {
                    let mut batch = writer.batched();
                    loop {
                        // Ensure there is room for the completion before consuming
                        // the IO so that we don't have to track completed IOs whose
                        // completions haven't been sent.
                        if !batch
                            .can_write(MAX_VMBUS_PACKET_SIZE)
                            .map_err(WorkerError::Queue)?
                        {
                            // This batch is full but there may still be more completions.
                            break;
                        }
                        if let Poll::Ready(Some((request_id, result, future))) =
                            self.scsi_requests.poll_next_unpin(cx)
                        {
                            self.future_pool.push(future);
                            self.handle_completion(&mut batch, request_id, result)?;
                            total_completions += 1;
                        } else {
                            tracing::trace!("out of completions");
                            break 'outer;
                        }
                    }
                }

                // Wait for enough space for any completion packets.
                if self.poll_for_ring_space(cx, &mut writer).is_pending() {
                    tracing::trace!("out of ring space");
                    break;
                }
            }

            let mut submissions = 0;
            // Process new requests.
            'outer: loop {
                if self.scsi_requests_states.len() >= self.max_io_queue_depth {
                    break;
                }
                let mut batch = if self.scsi_requests_states.len() < poll_mode_queue_depth {
                    if let Poll::Ready(batch) = reader.poll_read_batch(cx) {
                        batch.map_err(WorkerError::Queue)?
                    } else {
                        tracing::trace!("out of incoming packets");
                        break;
                    }
                } else {
                    match reader.try_read_batch() {
                        Ok(batch) => batch,
                        Err(queue::TryReadError::Empty) => {
                            tracing::trace!(
                                pending_io_count = self.scsi_requests_states.len(),
                                "out of incoming packets, keeping interrupts masked"
                            );
                            break;
                        }
                        Err(queue::TryReadError::Queue(err)) => Err(WorkerError::Queue(err))?,
                    }
                };

                let mut packets = batch.packets();
                loop {
                    if self.scsi_requests_states.len() >= self.max_io_queue_depth {
                        break 'outer;
                    }
                    // Wait for enough space for any completion packets that
                    // `handle_packet` may need to send, so that it isn't necessary
                    // to track pending completions.
                    if self.poll_for_ring_space(cx, &mut writer).is_pending() {
                        tracing::trace!("out of ring space");
                        break 'outer;
                    }

                    let packet = if let Some(packet) = packets.next() {
                        packet.map_err(WorkerError::Queue)?
                    } else {
                        break;
                    };

                    if self.handle_packet(&mut writer, &packet)? {
                        submissions += 1;
                    }
                }
            }

            // Loop around to poll the IOs if any new IOs were submitted.
            if submissions == 0 {
                // No need to poll again.
                break;
            }
            total_submissions += submissions;
        }

        if total_submissions != 0 || total_completions != 0 {
            self.stats.ios_submitted.add(total_submissions);
            self.stats
                .per_wake_submissions
                .add_sample(total_submissions);
            self.stats
                .per_wake_completions
                .add_sample(total_completions);
            self.stats.ios_completed.add(total_completions);
        } else {
            self.stats.wakes_spurious.increment();
        }

        Poll::Pending
    }

    fn handle_completion<M: RingMem>(
        &mut self,
        writer: &mut queue::WriteBatch<'_, M>,
        request_id: usize,
        result: ScsiResult,
    ) -> Result<(), WorkerError> {
        let state = self.scsi_requests_states.remove(request_id);
        let request_size = state.request.request_size;

        // Push the request into the pool to avoid reallocating later.
        assert_eq!(
            Arc::strong_count(&state.request) + Arc::weak_count(&state.request),
            1
        );
        self.full_request_pool.push(state.request);

        let status = convert_srb_status_to_nt_status(result.srb_status);
        let mut payload = [0; 0x14];
        if let Some(sense) = result.sense_data {
            payload[..size_of_val(&sense)].copy_from_slice(sense.as_bytes());
            tracing::trace!(sense_info = ?payload, sense_key = payload[2], asc = payload[12], "execute_scsi");
        };
        let response = storvsp_protocol::ScsiRequest {
            length: size_of::<storvsp_protocol::ScsiRequest>() as u16,
            scsi_status: result.scsi_status,
            srb_status: SrbStatusAndFlags::new()
                .with_status(result.srb_status)
                .with_autosense_valid(result.sense_data.is_some()),
            data_transfer_length: result.tx as u32,
            cdb_length: storvsp_protocol::CDB16GENERIC_LENGTH as u8,
            sense_info_ex_length: storvsp_protocol::VMSCSI_SENSE_BUFFER_SIZE as u8,
            payload,
            ..storvsp_protocol::ScsiRequest::new_zeroed()
        };
        self.send_vmbus_packet(
            writer,
            OutgoingPacketType::Completion,
            request_size,
            state.transaction_id,
            storvsp_protocol::Operation::COMPLETE_IO,
            status,
            response.as_bytes(),
        )?;
        Ok(())
    }

    fn handle_packet<M: RingMem>(
        &mut self,
        writer: &mut queue::WriteHalf<'_, M>,
        packet: &IncomingPacket<'_, M>,
    ) -> Result<bool, WorkerError> {
        let packet =
            parse_packet(packet, &mut self.full_request_pool).map_err(WorkerError::PacketError)?;
        let submitted_io = match packet.data {
            PacketData::ExecuteScsi(request) => {
                self.push_scsi_request(packet.transaction_id, request);
                true
            }
            PacketData::ResetAdapter | PacketData::ResetBus | PacketData::ResetLun => {
                // These operations have always been no-ops.
                self.send_completion(writer, &packet, storvsp_protocol::NtStatus::SUCCESS, &())?;
                false
            }
            PacketData::CreateSubChannels(new_subchannel_count) if self.channel_index == 0 => {
                if let Err(err) = self
                    .channel_control
                    .enable_subchannels(new_subchannel_count)
                {
                    tracelimit::warn_ratelimited!(?err, "cannot create subchannels");
                    self.send_completion(
                        writer,
                        &packet,
                        storvsp_protocol::NtStatus::INVALID_PARAMETER,
                        &(),
                    )?;
                    false
                } else {
                    // Update the subchannel count in the protocol state for save.
                    if let ProtocolState::Ready {
                        subchannel_count, ..
                    } = &mut *self.protocol.state.write()
                    {
                        *subchannel_count = new_subchannel_count;
                    } else {
                        unreachable!()
                    }

                    self.send_completion(
                        writer,
                        &packet,
                        storvsp_protocol::NtStatus::SUCCESS,
                        &(),
                    )?;
                    false
                }
            }
            _ => {
                tracelimit::warn_ratelimited!(data = ?packet.data, "unexpected packet on ready");
                self.send_completion(
                    writer,
                    &packet,
                    storvsp_protocol::NtStatus::INVALID_DEVICE_STATE,
                    &(),
                )?;
                false
            }
        };
        Ok(submitted_io)
    }

    fn push_scsi_request(&mut self, transaction_id: u64, full_request: Arc<ScsiRequestAndRange>) {
        let scsi_queue = self.scsi_queue.clone();
        let scsi_request_state = ScsiRequestState {
            transaction_id,
            request: full_request.clone(),
        };
        let request_id = self.scsi_requests_states.insert(scsi_request_state);
        let future = self
            .future_pool
            .pop()
            .unwrap_or_else(|| OversizedBox::new(()));
        let future = OversizedBox::refill(future, async move {
            scsi_queue.execute_scsi(full_request.as_ref()).await
        });
        let request = ScsiRequest::new(request_id, oversized_box::coerce!(future));
        self.scsi_requests.push(request);
    }
}

impl<T: RingMem> Drop for Worker<T> {
    fn drop(&mut self) {
        self.inner
            .controller
            .remove_rescan_notification_source(&self.rescan_notification);
    }
}

#[derive(Debug, Error)]
#[error("SCSI path {}:{}:{} is already in use", self.0.path, self.0.target, self.0.lun)]
pub struct ScsiPathInUse(pub ScsiPath);

#[derive(Debug, Error)]
#[error("SCSI path {}:{}:{} is not in use", self.0.path, self.0.target, self.0.lun)]
pub struct ScsiPathNotInUse(ScsiPath);

#[derive(Clone)]
struct ScsiRequestState {
    transaction_id: u64,
    request: Arc<ScsiRequestAndRange>,
}

#[derive(Debug)]
struct ScsiRequestAndRange {
    external_data: Range,
    external_data_buf: MultiPagedRangeBuf,
    request: storvsp_protocol::ScsiRequest,
    request_size: usize,
}

impl Inspect for ScsiRequestState {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond()
            .field("transaction_id", self.transaction_id)
            .display(
                "address",
                &ScsiPath {
                    path: self.request.request.path_id,
                    target: self.request.request.target_id,
                    lun: self.request.request.lun,
                },
            )
            .display_debug("operation", &ScsiOp(self.request.request.payload[0]));
    }
}

impl StorageDevice {
    /// Returns a new SCSI device.
    pub fn build_scsi(
        driver_source: &VmTaskDriverSource,
        controller: &ScsiController,
        instance_id: Guid,
        max_sub_channel_count: u16,
        io_queue_depth: u32,
    ) -> Self {
        Self::build_inner(
            driver_source,
            controller,
            instance_id,
            None,
            max_sub_channel_count,
            io_queue_depth,
        )
    }

    /// Returns a new SCSI device for implementing an IDE accelerator channel
    /// for IDE device `device_id` on channel `channel_id`.
    pub fn build_ide(
        driver_source: &VmTaskDriverSource,
        channel_id: u8,
        device_id: u8,
        disk: ScsiControllerDisk,
        io_queue_depth: u32,
    ) -> Self {
        let path = ScsiPath {
            path: channel_id,
            target: device_id,
            lun: 0,
        };

        let controller = ScsiController::new();
        controller.attach(path, disk).unwrap();

        // Construct the specific GUID that drivers in the guest expect for this
        // IDE device.
        let instance_id = Guid {
            data1: channel_id.into(),
            data2: device_id.into(),
            data3: 0x8899,
            data4: [0; 8],
        };
        Self::build_inner(
            driver_source,
            &controller,
            instance_id,
            Some(path),
            0,
            io_queue_depth,
        )
    }

    fn build_inner(
        driver_source: &VmTaskDriverSource,
        controller: &ScsiController,
        instance_id: Guid,
        ide_path: Option<ScsiPath>,
        max_sub_channel_count: u16,
        io_queue_depth: u32,
    ) -> Self {
        let workers = (0..max_sub_channel_count + 1)
            .map(|channel_index| WorkerAndDriver {
                worker: TaskControl::new(WorkerState),
                driver: driver_source
                    .builder()
                    .target_vp(0)
                    .run_on_target(true)
                    .build(format!("storvsp-{}-{}", instance_id, channel_index)),
            })
            .collect();

        Self {
            instance_id,
            ide_path,
            workers,
            controller: controller.state.clone(),
            resources: Default::default(),
            max_sub_channel_count,
            driver_source: driver_source.clone(),
            protocol: Arc::new(Protocol {
                state: RwLock::new(ProtocolState::Init(InitState::Begin)),
                ready: Default::default(),
            }),
            io_queue_depth,
        }
    }

    fn new_worker(
        &mut self,
        open_request: &OpenRequest,
        channel_index: u16,
    ) -> anyhow::Result<&mut Worker> {
        let controller = self.controller.clone();

        let driver = self
            .driver_source
            .builder()
            .target_vp(open_request.open_data.target_vp)
            .run_on_target(true)
            .build(format!("storvsp-{}-{}", self.instance_id, channel_index));

        let channel = gpadl_channel(&driver, &self.resources, open_request, channel_index)
            .context("failed to create vmbus channel")?;

        let channel_control = self.resources.channel_control.clone();

        tracing::debug!(
            target_vp = open_request.open_data.target_vp,
            channel_index,
            "packet processing starting...",
        );

        // Force the path ID on incoming SCSI requests to match the IDE
        // channel ID, since guests do not reliably set the path ID
        // correctly.
        let force_path_id = self.ide_path.map(|p| p.path);

        let worker = Worker::new(
            controller,
            channel,
            channel_index,
            self.resources
                .offer_resources
                .guest_memory(open_request)
                .clone(),
            channel_control,
            self.io_queue_depth,
            self.protocol.clone(),
            force_path_id,
        )
        .map_err(RestoreError::Other)?;

        self.workers[channel_index as usize]
            .driver
            .retarget_vp(open_request.open_data.target_vp);

        Ok(self.workers[channel_index as usize].worker.insert(
            &driver,
            format!("storvsp worker {}-{}", self.instance_id, channel_index),
            worker,
        ))
    }
}

/// A disk that can be added to a SCSI controller.
#[derive(Clone)]
pub struct ScsiControllerDisk {
    disk: Arc<dyn AsyncScsiDisk>,
}

impl ScsiControllerDisk {
    /// Creates a new controller disk from an async SCSI disk.
    pub fn new(disk: Arc<dyn AsyncScsiDisk>) -> Self {
        Self { disk }
    }
}

struct ScsiControllerState {
    disks: RwLock<HashMap<ScsiPath, ScsiControllerDisk>>,
    rescan_notification_source: Mutex<Vec<futures::channel::mpsc::Sender<()>>>,
    poll_mode_queue_depth: AtomicU32,
}

pub struct ScsiController {
    state: Arc<ScsiControllerState>,
}

impl ScsiController {
    pub fn new() -> Self {
        Self::new_with_poll_mode_queue_depth(None)
    }

    pub fn new_with_poll_mode_queue_depth(poll_mode_queue_depth: Option<u32>) -> Self {
        Self {
            state: Arc::new(ScsiControllerState {
                disks: Default::default(),
                rescan_notification_source: Mutex::new(Vec::new()),
                poll_mode_queue_depth: AtomicU32::new(
                    poll_mode_queue_depth.unwrap_or(DEFAULT_POLL_MODE_QUEUE_DEPTH),
                ),
            }),
        }
    }

    pub fn attach(&self, path: ScsiPath, disk: ScsiControllerDisk) -> Result<(), ScsiPathInUse> {
        match self.state.disks.write().entry(path) {
            Entry::Occupied(_) => return Err(ScsiPathInUse(path)),
            Entry::Vacant(entry) => entry.insert(disk),
        };
        for source in self.state.rescan_notification_source.lock().iter_mut() {
            // Ok to ignore errors here. If the channel is full a previous notification has not yet
            // been processed by the primary channel worker.
            source.try_send(()).ok();
        }
        Ok(())
    }

    pub fn remove(&self, path: ScsiPath) -> Result<(), ScsiPathNotInUse> {
        match self.state.disks.write().entry(path) {
            Entry::Vacant(_) => return Err(ScsiPathNotInUse(path)),
            Entry::Occupied(entry) => {
                entry.remove();
            }
        }
        for source in self.state.rescan_notification_source.lock().iter_mut() {
            // Ok to ignore errors here. If the channel is full a previous notification has not yet
            // been processed by the primary channel worker.
            source.try_send(()).ok();
        }
        Ok(())
    }
}

impl ScsiControllerState {
    fn add_rescan_notification_source(&self, source: futures::channel::mpsc::Sender<()>) {
        self.rescan_notification_source.lock().push(source);
    }

    fn remove_rescan_notification_source(&self, target: &futures::channel::mpsc::Receiver<()>) {
        let mut sources = self.rescan_notification_source.lock();
        if let Some(index) = sources
            .iter()
            .position(|source| source.is_connected_to(target))
        {
            sources.remove(index);
        }
    }
}

#[async_trait]
impl VmbusDevice for StorageDevice {
    fn offer(&self) -> OfferParams {
        if let Some(path) = self.ide_path {
            let offer_properties = storvsp_protocol::OfferProperties {
                path_id: path.path,
                target_id: path.target,
                flags: storvsp_protocol::OFFER_PROPERTIES_FLAG_IDE_DEVICE,
                ..FromZeros::new_zeroed()
            };
            let mut user_defined = UserDefinedData::new_zeroed();
            offer_properties
                .write_to_prefix(&mut user_defined[..])
                .unwrap();
            OfferParams {
                interface_name: "ide-accel".to_owned(),
                instance_id: self.instance_id,
                interface_id: storvsp_protocol::IDE_ACCELERATOR_INTERFACE_ID,
                channel_type: ChannelType::Interface { user_defined },
                ..Default::default()
            }
        } else {
            OfferParams {
                interface_name: "scsi".to_owned(),
                instance_id: self.instance_id,
                interface_id: storvsp_protocol::SCSI_INTERFACE_ID,
                ..Default::default()
            }
        }
    }

    fn max_subchannels(&self) -> u16 {
        self.max_sub_channel_count
    }

    fn install(&mut self, resources: DeviceResources) {
        self.resources = resources;
    }

    async fn open(
        &mut self,
        channel_index: u16,
        open_request: &OpenRequest,
    ) -> Result<(), ChannelOpenError> {
        tracing::debug!(channel_index, "scsi open channel");
        self.new_worker(open_request, channel_index)?;
        self.workers[channel_index as usize].worker.start();
        Ok(())
    }

    async fn close(&mut self, channel_index: u16) {
        tracing::debug!(channel_index, "scsi close channel");
        let worker = &mut self.workers[channel_index as usize].worker;
        worker.stop().await;
        if worker.state_mut().is_some() {
            worker
                .state_mut()
                .unwrap()
                .wait_for_scsi_requests_complete()
                .await;
            worker.remove();
        }
        if channel_index == 0 {
            *self.protocol.state.write() = ProtocolState::Init(InitState::Begin);
        }
    }

    async fn retarget_vp(&mut self, channel_index: u16, target_vp: u32) {
        self.workers[channel_index as usize]
            .driver
            .retarget_vp(target_vp);
    }

    fn start(&mut self) {
        for task in self
            .workers
            .iter_mut()
            .filter(|task| task.worker.has_state() && !task.worker.is_running())
        {
            task.worker.start();
        }
    }

    async fn stop(&mut self) {
        tracing::debug!(instance_id = ?self.instance_id, "StorageDevice stopping...");
        for task in self
            .workers
            .iter_mut()
            .filter(|task| task.worker.has_state() && task.worker.is_running())
        {
            task.worker.stop().await;
        }
    }

    fn supports_save_restore(&mut self) -> Option<&mut dyn SaveRestoreVmbusDevice> {
        Some(self)
    }
}

#[async_trait]
impl SaveRestoreVmbusDevice for StorageDevice {
    async fn save(&mut self) -> Result<SavedStateBlob, SaveError> {
        Ok(SavedStateBlob::new(self.save()?))
    }

    async fn restore(
        &mut self,
        control: RestoreControl<'_>,
        state: SavedStateBlob,
    ) -> Result<(), RestoreError> {
        self.restore(control, state.parse()?).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::TestWorker;
    use crate::test_helpers::parse_guest_completion;
    use crate::test_helpers::parse_guest_completion_check_flags_status;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use scsi::srb::SrbStatus;
    use test_with_tracing::test;
    use vmbus_channel::connected_async_channels;

    // Discourage `Clone` for `ScsiController` outside the crate, but it is
    // necessary for testing. The fuzzer also uses `TestWorker`, which needs
    // a `clone` of the inner state, but is not in this crate.
    impl Clone for ScsiController {
        fn clone(&self) -> Self {
            ScsiController {
                state: self.state.clone(),
            }
        }
    }

    #[async_test]
    async fn test_channel_working(driver: DefaultDriver) {
        // set up the channels and worker
        let (host, guest) = connected_async_channels(16 * 1024);
        let guest_queue = Queue::new(guest).unwrap();

        let test_guest_mem = GuestMemory::allocate(16384);
        let controller = ScsiController::new();
        let disk = scsidisk::SimpleScsiDisk::new(
            disklayer_ram::ram_disk(10 * 1024 * 1024, false).unwrap(),
            Default::default(),
        );
        controller
            .attach(
                ScsiPath {
                    path: 0,
                    target: 0,
                    lun: 0,
                },
                ScsiControllerDisk::new(Arc::new(disk)),
            )
            .unwrap();

        let test_worker = TestWorker::start(
            controller.clone(),
            driver.clone(),
            test_guest_mem.clone(),
            host,
            None,
        );

        let mut guest = test_helpers::TestGuest {
            queue: guest_queue,
            transaction_id: 0,
        };

        guest.perform_protocol_negotiation().await;

        // Set up the buffer for a write request
        const IO_LEN: usize = 4 * 1024;
        let write_buf = [7u8; IO_LEN];
        let write_gpa = 4 * 1024u64;
        test_guest_mem.write_at(write_gpa, &write_buf).unwrap();
        guest
            .send_write_packet(ScsiPath::default(), write_gpa, 1, IO_LEN)
            .await;

        guest
            .verify_completion(|p| test_helpers::parse_guest_completed_io(p, SrbStatus::SUCCESS))
            .await;

        let read_gpa = 8 * 1024u64;
        guest
            .send_read_packet(ScsiPath::default(), read_gpa, 1, IO_LEN)
            .await;

        guest
            .verify_completion(|p| test_helpers::parse_guest_completed_io(p, SrbStatus::SUCCESS))
            .await;
        let mut read_buf = [0u8; IO_LEN];
        test_guest_mem.read_at(read_gpa, &mut read_buf).unwrap();
        for (b1, b2) in read_buf.iter().zip(write_buf.iter()) {
            assert_eq!(b1, b2);
        }

        // stop everything
        guest.verify_graceful_close(test_worker).await;
    }

    #[async_test]
    async fn test_packet_sizes(driver: DefaultDriver) {
        // set up the channels and worker
        let (host, guest) = connected_async_channels(16384);
        let guest_queue = Queue::new(guest).unwrap();

        let test_guest_mem = GuestMemory::allocate(1024);
        let controller = ScsiController::new();

        let _worker = TestWorker::start(
            controller.clone(),
            driver.clone(),
            test_guest_mem,
            host,
            None,
        );

        let mut guest = test_helpers::TestGuest {
            queue: guest_queue,
            transaction_id: 0,
        };

        let negotiate_packet = storvsp_protocol::Packet {
            operation: storvsp_protocol::Operation::BEGIN_INITIALIZATION,
            flags: 0,
            status: storvsp_protocol::NtStatus::SUCCESS,
        };
        guest
            .send_data_packet_sync(&[negotiate_packet.as_bytes()])
            .await;

        guest.verify_completion(parse_guest_completion).await;

        let header = storvsp_protocol::Packet {
            operation: storvsp_protocol::Operation::QUERY_PROTOCOL_VERSION,
            flags: 0,
            status: storvsp_protocol::NtStatus::SUCCESS,
        };

        let mut buf = [0u8; 128];
        storvsp_protocol::ProtocolVersion {
            major_minor: !0,
            reserved: 0,
        }
        .write_to_prefix(&mut buf[..])
        .unwrap(); // PANIC: Infallable since `ProtcolVersion` is less than 128 bytes

        for &(len, resp_len) in &[(48, 48), (50, 56), (56, 56), (64, 64), (72, 64)] {
            guest
                .send_data_packet_sync(&[header.as_bytes(), &buf[..len - size_of_val(&header)]])
                .await;

            guest
                .verify_completion(|packet| {
                    let IncomingPacket::Completion(packet) = packet else {
                        unreachable!()
                    };
                    assert_eq!(packet.reader().len(), resp_len);
                    assert_eq!(
                        packet
                            .reader()
                            .read_plain::<storvsp_protocol::Packet>()
                            .unwrap()
                            .status,
                        storvsp_protocol::NtStatus::REVISION_MISMATCH
                    );
                    Ok(())
                })
                .await;
        }
    }

    #[async_test]
    async fn test_wrong_first_packet(driver: DefaultDriver) {
        // set up the channels and worker
        let (host, guest) = connected_async_channels(16384);
        let guest_queue = Queue::new(guest).unwrap();

        let test_guest_mem = GuestMemory::allocate(1024);
        let controller = ScsiController::new();

        let _worker = TestWorker::start(
            controller.clone(),
            driver.clone(),
            test_guest_mem,
            host,
            None,
        );

        let mut guest = test_helpers::TestGuest {
            queue: guest_queue,
            transaction_id: 0,
        };

        // Protocol negotiation done out of order
        let negotiate_packet = storvsp_protocol::Packet {
            operation: storvsp_protocol::Operation::END_INITIALIZATION,
            flags: 0,
            status: storvsp_protocol::NtStatus::SUCCESS,
        };
        guest
            .send_data_packet_sync(&[negotiate_packet.as_bytes()])
            .await;

        guest
            .verify_completion(|packet| {
                let IncomingPacket::Completion(packet) = packet else {
                    unreachable!()
                };
                assert_eq!(
                    packet
                        .reader()
                        .read_plain::<storvsp_protocol::Packet>()
                        .unwrap()
                        .status,
                    storvsp_protocol::NtStatus::INVALID_DEVICE_STATE
                );
                Ok(())
            })
            .await;
    }

    #[async_test]
    async fn test_unrecognized_operation(driver: DefaultDriver) {
        // set up the channels and worker
        let (host, guest) = connected_async_channels(16384);
        let guest_queue = Queue::new(guest).unwrap();

        let test_guest_mem = GuestMemory::allocate(1024);
        let controller = ScsiController::new();

        let worker = TestWorker::start(
            controller.clone(),
            driver.clone(),
            test_guest_mem,
            host,
            None,
        );

        let mut guest = test_helpers::TestGuest {
            queue: guest_queue,
            transaction_id: 0,
        };

        // Send packet with unrecognized operation
        let negotiate_packet = storvsp_protocol::Packet {
            operation: storvsp_protocol::Operation::REMOVE_DEVICE,
            flags: 0,
            status: storvsp_protocol::NtStatus::SUCCESS,
        };
        guest
            .send_data_packet_sync(&[negotiate_packet.as_bytes()])
            .await;

        match worker.teardown().await {
            Err(WorkerError::PacketError(PacketError::UnrecognizedOperation(
                storvsp_protocol::Operation::REMOVE_DEVICE,
            ))) => {}
            result => panic!("Worker failed with unexpected result {:?}!", result),
        }
    }

    #[async_test]
    async fn test_too_many_subchannels(driver: DefaultDriver) {
        // set up the channels and worker
        let (host, guest) = connected_async_channels(16384);
        let guest_queue = Queue::new(guest).unwrap();

        let test_guest_mem = GuestMemory::allocate(1024);
        let controller = ScsiController::new();

        let _worker = TestWorker::start(
            controller.clone(),
            driver.clone(),
            test_guest_mem,
            host,
            None,
        );

        let mut guest = test_helpers::TestGuest {
            queue: guest_queue,
            transaction_id: 0,
        };

        let negotiate_packet = storvsp_protocol::Packet {
            operation: storvsp_protocol::Operation::BEGIN_INITIALIZATION,
            flags: 0,
            status: storvsp_protocol::NtStatus::SUCCESS,
        };
        guest
            .send_data_packet_sync(&[negotiate_packet.as_bytes()])
            .await;
        guest.verify_completion(parse_guest_completion).await;

        let version_packet = storvsp_protocol::Packet {
            operation: storvsp_protocol::Operation::QUERY_PROTOCOL_VERSION,
            flags: 0,
            status: storvsp_protocol::NtStatus::SUCCESS,
        };
        let version = storvsp_protocol::ProtocolVersion {
            major_minor: storvsp_protocol::VERSION_BLUE,
            reserved: 0,
        };
        guest
            .send_data_packet_sync(&[version_packet.as_bytes(), version.as_bytes()])
            .await;
        guest.verify_completion(parse_guest_completion).await;

        let properties_packet = storvsp_protocol::Packet {
            operation: storvsp_protocol::Operation::QUERY_PROPERTIES,
            flags: 0,
            status: storvsp_protocol::NtStatus::SUCCESS,
        };
        guest
            .send_data_packet_sync(&[properties_packet.as_bytes()])
            .await;

        guest.verify_completion(parse_guest_completion).await;

        let negotiate_packet = storvsp_protocol::Packet {
            operation: storvsp_protocol::Operation::CREATE_SUB_CHANNELS,
            flags: 0,
            status: storvsp_protocol::NtStatus::SUCCESS,
        };
        // Create sub channels more than maximum_sub_channel_count
        guest
            .send_data_packet_sync(&[negotiate_packet.as_bytes(), 1_u16.as_bytes()])
            .await;

        guest
            .verify_completion(|packet| {
                let IncomingPacket::Completion(packet) = packet else {
                    unreachable!()
                };
                assert_eq!(
                    packet
                        .reader()
                        .read_plain::<storvsp_protocol::Packet>()
                        .unwrap()
                        .status,
                    storvsp_protocol::NtStatus::INVALID_PARAMETER
                );
                Ok(())
            })
            .await;
    }

    #[async_test]
    async fn test_begin_init_on_ready(driver: DefaultDriver) {
        // set up the channels and worker
        let (host, guest) = connected_async_channels(16384);
        let guest_queue = Queue::new(guest).unwrap();

        let test_guest_mem = GuestMemory::allocate(1024);
        let controller = ScsiController::new();

        let _worker = TestWorker::start(
            controller.clone(),
            driver.clone(),
            test_guest_mem,
            host,
            None,
        );

        let mut guest = test_helpers::TestGuest {
            queue: guest_queue,
            transaction_id: 0,
        };

        guest.perform_protocol_negotiation().await;

        // Protocol negotiation done out of order
        let negotiate_packet = storvsp_protocol::Packet {
            operation: storvsp_protocol::Operation::BEGIN_INITIALIZATION,
            flags: 0,
            status: storvsp_protocol::NtStatus::SUCCESS,
        };
        guest
            .send_data_packet_sync(&[negotiate_packet.as_bytes()])
            .await;

        guest
            .verify_completion(|p| {
                parse_guest_completion_check_flags_status(
                    p,
                    0,
                    storvsp_protocol::NtStatus::INVALID_DEVICE_STATE,
                )
            })
            .await;
    }

    #[async_test]
    async fn test_hot_add_remove(driver: DefaultDriver) {
        // set up channels and worker.
        let (host, guest) = connected_async_channels(16 * 1024);
        let guest_queue = Queue::new(guest).unwrap();

        let test_guest_mem = GuestMemory::allocate(16384);
        // create a controller with no disk yet.
        let controller = ScsiController::new();

        let test_worker = TestWorker::start(
            controller.clone(),
            driver.clone(),
            test_guest_mem.clone(),
            host,
            None,
        );

        let mut guest = test_helpers::TestGuest {
            queue: guest_queue,
            transaction_id: 0,
        };

        guest.perform_protocol_negotiation().await;

        // Verify no LUNs are reported initially.
        let mut lun_list_buffer: [u8; 256] = [0; 256];
        let mut disk_count = 0;
        guest
            .send_report_luns_packet(ScsiPath::default(), 0, lun_list_buffer.len())
            .await;
        guest
            .verify_completion(|p| {
                test_helpers::parse_guest_completed_io_check_tx_len(p, SrbStatus::SUCCESS, Some(8))
            })
            .await;
        test_guest_mem.read_at(0, &mut lun_list_buffer).unwrap();
        let lun_list_size = u32::from_be_bytes(lun_list_buffer[0..4].try_into().unwrap());
        assert_eq!(lun_list_size, disk_count as u32 * 8);

        // Set up a buffer for writes.
        const IO_LEN: usize = 4 * 1024;
        let write_buf = [7u8; IO_LEN];
        let write_gpa = 4 * 1024u64;
        test_guest_mem.write_at(write_gpa, &write_buf).unwrap();

        guest
            .send_write_packet(ScsiPath::default(), write_gpa, 1, IO_LEN)
            .await;
        guest
            .verify_completion(|p| {
                test_helpers::parse_guest_completed_io(p, SrbStatus::INVALID_LUN)
            })
            .await;

        // Add some disks while the guest is running.
        for lun in 0..4 {
            let disk = scsidisk::SimpleScsiDisk::new(
                disklayer_ram::ram_disk(10 * 1024 * 1024, false).unwrap(),
                Default::default(),
            );
            controller
                .attach(
                    ScsiPath {
                        path: 0,
                        target: 0,
                        lun,
                    },
                    ScsiControllerDisk::new(Arc::new(disk)),
                )
                .unwrap();
            guest
                .verify_completion(test_helpers::parse_guest_enumerate_bus)
                .await;

            disk_count += 1;
            guest
                .send_report_luns_packet(ScsiPath::default(), 0, 256)
                .await;
            guest
                .verify_completion(|p| {
                    test_helpers::parse_guest_completed_io_check_tx_len(
                        p,
                        SrbStatus::SUCCESS,
                        Some((disk_count + 1) * 8),
                    )
                })
                .await;
            test_guest_mem.read_at(0, &mut lun_list_buffer).unwrap();
            let lun_list_size = u32::from_be_bytes(lun_list_buffer[0..4].try_into().unwrap());
            assert_eq!(lun_list_size, disk_count as u32 * 8);

            guest
                .send_write_packet(
                    ScsiPath {
                        path: 0,
                        target: 0,
                        lun,
                    },
                    write_gpa,
                    1,
                    IO_LEN,
                )
                .await;
            guest
                .verify_completion(|p| {
                    test_helpers::parse_guest_completed_io(p, SrbStatus::SUCCESS)
                })
                .await;
        }

        // Remove all disks while the guest is running.
        for lun in 0..4 {
            controller
                .remove(ScsiPath {
                    path: 0,
                    target: 0,
                    lun,
                })
                .unwrap();
            guest
                .verify_completion(test_helpers::parse_guest_enumerate_bus)
                .await;

            disk_count -= 1;
            guest
                .send_report_luns_packet(ScsiPath::default(), 0, 4096)
                .await;
            guest
                .verify_completion(|p| {
                    test_helpers::parse_guest_completed_io_check_tx_len(
                        p,
                        SrbStatus::SUCCESS,
                        Some((disk_count + 1) * 8),
                    )
                })
                .await;
            test_guest_mem.read_at(0, &mut lun_list_buffer).unwrap();
            let lun_list_size = u32::from_be_bytes(lun_list_buffer[0..4].try_into().unwrap());
            assert_eq!(lun_list_size, disk_count as u32 * 8);

            guest
                .send_write_packet(
                    ScsiPath {
                        path: 0,
                        target: 0,
                        lun,
                    },
                    write_gpa,
                    1,
                    IO_LEN,
                )
                .await;
            guest
                .verify_completion(|p| {
                    test_helpers::parse_guest_completed_io(p, SrbStatus::INVALID_LUN)
                })
                .await;
        }

        guest.verify_graceful_close(test_worker).await;
    }

    #[async_test]
    pub async fn test_async_disk(driver: DefaultDriver) {
        let device = disklayer_ram::ram_disk(64 * 1024, false).unwrap();
        let controller = ScsiController::new();
        let disk = ScsiControllerDisk::new(Arc::new(scsidisk::SimpleScsiDisk::new(
            device,
            Default::default(),
        )));
        controller
            .attach(
                ScsiPath {
                    path: 0,
                    target: 0,
                    lun: 0,
                },
                disk,
            )
            .unwrap();

        let (host, guest) = connected_async_channels(16 * 1024);
        let guest_queue = Queue::new(guest).unwrap();

        let mut guest = test_helpers::TestGuest {
            queue: guest_queue,
            transaction_id: 0,
        };

        let test_guest_mem = GuestMemory::allocate(16384);
        let worker = TestWorker::start(
            controller.clone(),
            &driver,
            test_guest_mem.clone(),
            host,
            None,
        );

        let negotiate_packet = storvsp_protocol::Packet {
            operation: storvsp_protocol::Operation::BEGIN_INITIALIZATION,
            flags: 0,
            status: storvsp_protocol::NtStatus::SUCCESS,
        };
        guest
            .send_data_packet_sync(&[negotiate_packet.as_bytes()])
            .await;
        guest.verify_completion(parse_guest_completion).await;

        let version_packet = storvsp_protocol::Packet {
            operation: storvsp_protocol::Operation::QUERY_PROTOCOL_VERSION,
            flags: 0,
            status: storvsp_protocol::NtStatus::SUCCESS,
        };
        let version = storvsp_protocol::ProtocolVersion {
            major_minor: storvsp_protocol::VERSION_BLUE,
            reserved: 0,
        };
        guest
            .send_data_packet_sync(&[version_packet.as_bytes(), version.as_bytes()])
            .await;
        guest.verify_completion(parse_guest_completion).await;

        let properties_packet = storvsp_protocol::Packet {
            operation: storvsp_protocol::Operation::QUERY_PROPERTIES,
            flags: 0,
            status: storvsp_protocol::NtStatus::SUCCESS,
        };
        guest
            .send_data_packet_sync(&[properties_packet.as_bytes()])
            .await;
        guest.verify_completion(parse_guest_completion).await;

        let negotiate_packet = storvsp_protocol::Packet {
            operation: storvsp_protocol::Operation::END_INITIALIZATION,
            flags: 0,
            status: storvsp_protocol::NtStatus::SUCCESS,
        };
        guest
            .send_data_packet_sync(&[negotiate_packet.as_bytes()])
            .await;
        guest.verify_completion(parse_guest_completion).await;

        const IO_LEN: usize = 4 * 1024;
        let write_buf = [7u8; IO_LEN];
        let write_gpa = 4 * 1024u64;
        test_guest_mem.write_at(write_gpa, &write_buf).unwrap();
        guest
            .send_write_packet(ScsiPath::default(), write_gpa, 1, IO_LEN)
            .await;
        guest
            .verify_completion(|p| test_helpers::parse_guest_completed_io(p, SrbStatus::SUCCESS))
            .await;

        let read_gpa = 8 * 1024u64;
        guest
            .send_read_packet(ScsiPath::default(), read_gpa, 1, IO_LEN)
            .await;
        guest
            .verify_completion(|p| test_helpers::parse_guest_completed_io(p, SrbStatus::SUCCESS))
            .await;
        let mut read_buf = [0u8; IO_LEN];
        test_guest_mem.read_at(read_gpa, &mut read_buf).unwrap();
        for (b1, b2) in read_buf.iter().zip(write_buf.iter()) {
            assert_eq!(b1, b2);
        }

        guest.verify_graceful_close(worker).await;
    }
}
