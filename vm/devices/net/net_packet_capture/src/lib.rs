// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `pcapng` compatible packet capture endpoint implementation.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

use async_trait::async_trait;
use futures::FutureExt;
use futures::StreamExt;
use futures::lock::Mutex;
use futures_concurrency::future::Race;
use guestmem::GuestMemory;
use inspect::InspectMut;
use mesh::error::RemoteError;
use mesh::rpc::FailableRpc;
use mesh::rpc::RpcSend;
use net_backend::BufferAccess;
use net_backend::Endpoint;
use net_backend::EndpointAction;
use net_backend::MultiQueueSupport;
use net_backend::Queue;
use net_backend::QueueConfig;
use net_backend::RssConfig;
use net_backend::RxId;
use net_backend::TxError;
use net_backend::TxId;
use net_backend::TxOffloadSupport;
use net_backend::TxSegment;
use net_backend::next_packet;
use pcap_file::DataLink;
use pcap_file::PcapError;
use pcap_file::PcapResult;
use pcap_file::pcapng::PcapNgWriter;
use pcap_file::pcapng::blocks::enhanced_packet::EnhancedPacketBlock;
use pcap_file::pcapng::blocks::interface_description::InterfaceDescriptionBlock;
use std::borrow::Cow;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Defines packet capture operations.
#[derive(Debug, PartialEq, mesh::MeshPayload)]
pub enum PacketCaptureOperation {
    /// Query details.
    Query,
    /// Start packet capture.
    Start,
    /// Stop packet capture.
    Stop,
}

/// Defines start operation data.
#[derive(Debug, mesh::MeshPayload)]
pub struct StartData<W: Write> {
    pub snaplen: u32,
    pub writers: Vec<W>,
}

/// Defines operational data.
#[derive(Debug, mesh::MeshPayload)]
pub enum OperationData<W: Write> {
    OpQueryData(u32),
    OpStartData(StartData<W>),
}

/// Additional parameters provided as part of a network packet capture trace.
#[derive(Debug, mesh::MeshPayload)]
pub struct PacketCaptureParams<W: Write> {
    /// Indicates the network capture operation.
    pub operation: PacketCaptureOperation,
    /// Operational data that is specific to the given operation.
    pub op_data: Option<OperationData<W>>,
}

trait PcapWriter: Send + Sync {
    /// Writes a EnhancedPacketBlocke
    fn write_pcapng_block_eb(&mut self, block: EnhancedPacketBlock<'_>) -> PcapResult<usize>;

    /// Writes a InterfaceDescriptionBlock
    fn write_pcapng_block_id(&mut self, block: InterfaceDescriptionBlock<'_>) -> PcapResult<usize>;
}

struct LocalPcapWriter<W: Write> {
    inner: PcapNgWriter<W>,
}

impl<W: Write + Send + Sync> PcapWriter for LocalPcapWriter<W> {
    fn write_pcapng_block_eb(&mut self, block: EnhancedPacketBlock<'_>) -> PcapResult<usize> {
        self.inner.write_pcapng_block(block)
    }

    fn write_pcapng_block_id(&mut self, block: InterfaceDescriptionBlock<'_>) -> PcapResult<usize> {
        self.inner.write_pcapng_block(block)
    }
}

struct PacketCaptureOptions {
    operation: PacketCaptureOperation,
    snaplen: usize,
    writer: Option<Box<dyn PcapWriter>>,
}

impl PacketCaptureOptions {
    fn new_with_start<W: Write + Send + Sync + 'static>(snaplen: u32, writer: W) -> Self {
        //TODO: Native endianness?
        let pcap_ng_writer =
            PcapNgWriter::with_endianness(writer, pcap_file::Endianness::Big).unwrap();

        let local_writer = LocalPcapWriter {
            inner: pcap_ng_writer,
        };

        Self {
            operation: PacketCaptureOperation::Start,
            snaplen: snaplen as usize,
            writer: Some(Box::new(local_writer)),
        }
    }

    fn new_with_stop() -> Self {
        Self {
            operation: PacketCaptureOperation::Stop,
            snaplen: 0,
            writer: None,
        }
    }
}

enum PacketCaptureEndpointCommand {
    PacketCapture(FailableRpc<PacketCaptureOptions, ()>),
}

pub struct PacketCaptureEndpointControl {
    control_tx: mesh::Sender<PacketCaptureEndpointCommand>,
}

impl PacketCaptureEndpointControl {
    pub async fn packet_capture<W: Write + Send + Sync + 'static>(
        &self,
        params: PacketCaptureParams<W>,
    ) -> anyhow::Result<PacketCaptureParams<W>> {
        let mut params = params;
        let options = match params.operation {
            PacketCaptureOperation::Query | PacketCaptureOperation::Start => {
                let Some(op_data) = &mut params.op_data else {
                    anyhow::bail!(
                        "Invalid input parameter. Expecting operational data, but none provided"
                    );
                };

                match op_data {
                    OperationData::OpQueryData(num_streams) => {
                        return Ok(PacketCaptureParams {
                            operation: params.operation,
                            op_data: Some(OperationData::OpQueryData(*num_streams + 1)),
                        });
                    }
                    OperationData::OpStartData(data) => {
                        if data.writers.is_empty() {
                            anyhow::bail!("Insufficient streams");
                        }
                        let socket = data.writers.remove(0);
                        PacketCaptureOptions::new_with_start(data.snaplen, socket)
                    }
                }
            }
            PacketCaptureOperation::Stop => PacketCaptureOptions::new_with_stop(),
        };

        self.control_tx
            .call_failable(PacketCaptureEndpointCommand::PacketCapture, options)
            .await?;

        Ok(params)
    }
}

pub struct PacketCaptureEndpoint {
    /// Some identifier that this endpoint can identify itself using for things
    /// like tracing, filtering etc..
    id: String,
    endpoint: Box<dyn Endpoint>,
    control_rx: Arc<Mutex<mesh::Receiver<PacketCaptureEndpointCommand>>>,
    pcap: Arc<Pcap>,
}

impl InspectMut for PacketCaptureEndpoint {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        self.current_mut().inspect_mut(req)
    }
}

impl PacketCaptureEndpoint {
    pub fn new(endpoint: Box<dyn Endpoint>, id: String) -> (Self, PacketCaptureEndpointControl) {
        let (control_tx, control_rx) = mesh::channel();
        let control = PacketCaptureEndpointControl {
            control_tx: control_tx.clone(),
        };
        let pcap = Arc::new(Pcap::new(control_tx.clone()));
        (
            Self {
                id,
                endpoint,
                control_rx: Arc::new(Mutex::new(control_rx)),
                pcap,
            },
            control,
        )
    }

    fn current(&self) -> &dyn Endpoint {
        self.endpoint.as_ref()
    }

    fn current_mut(&mut self) -> &mut dyn Endpoint {
        self.endpoint.as_mut()
    }
}

#[async_trait]
impl Endpoint for PacketCaptureEndpoint {
    fn endpoint_type(&self) -> &'static str {
        self.current().endpoint_type()
    }

    async fn get_queues(
        &mut self,
        config: Vec<QueueConfig<'_>>,
        rss: Option<&RssConfig<'_>>,
        queues: &mut Vec<Box<dyn Queue>>,
    ) -> anyhow::Result<()> {
        if self.pcap.enabled.load(Ordering::Relaxed) {
            tracing::trace!("using packet capture queues");
            let mem = config[0].pool.guest_memory().clone();
            let mut queues_inner: Vec<Box<dyn Queue>> = Vec::new();
            self.current_mut()
                .get_queues(config, rss, &mut queues_inner)
                .await?;
            while let Some(inner) = queues_inner.pop() {
                queues.push(Box::new(PacketCaptureQueue {
                    queue: inner,
                    mem: mem.clone(),
                    pcap: self.pcap.clone(),
                }));
            }
        } else {
            tracing::trace!("using inner queues");
            self.current_mut().get_queues(config, rss, queues).await?;
        }
        Ok(())
    }

    async fn stop(&mut self) {
        self.current_mut().stop().await
    }

    fn is_ordered(&self) -> bool {
        self.current().is_ordered()
    }

    fn tx_offload_support(&self) -> TxOffloadSupport {
        self.current().tx_offload_support()
    }

    fn multiqueue_support(&self) -> MultiQueueSupport {
        self.current().multiqueue_support()
    }

    fn tx_fast_completions(&self) -> bool {
        self.current().tx_fast_completions()
    }

    async fn set_data_path_to_guest_vf(&self, use_vf: bool) -> anyhow::Result<()> {
        self.current().set_data_path_to_guest_vf(use_vf).await
    }

    async fn get_data_path_to_guest_vf(&self) -> anyhow::Result<bool> {
        self.current().get_data_path_to_guest_vf().await
    }

    async fn wait_for_endpoint_action(&mut self) -> EndpointAction {
        enum Message {
            PacketCaptureEndpointCommand(PacketCaptureEndpointCommand),
            UpdateFromEndpoint(EndpointAction),
        }
        loop {
            let receiver = self.control_rx.clone();
            let mut receive_update = receiver.lock().await;
            let update = async {
                match receive_update.next().await {
                    Some(m) => Message::PacketCaptureEndpointCommand(m),
                    None => {
                        std::future::pending::<()>().await;
                        unreachable!()
                    }
                }
            };
            let ep_update = self
                .current_mut()
                .wait_for_endpoint_action()
                .map(Message::UpdateFromEndpoint);
            let m = (update, ep_update).race().await;
            match m {
                Message::PacketCaptureEndpointCommand(
                    PacketCaptureEndpointCommand::PacketCapture(rpc),
                ) => {
                    let (options, response) = rpc.split();
                    let result = async {
                        let id = &self.id;
                        let start = match options.operation {
                            PacketCaptureOperation::Start => {
                                tracing::info!(id, "starting trace");
                                true
                            }
                            PacketCaptureOperation::Stop => {
                                tracing::info!(id, "stopping trace");
                                false
                            }
                            _ => Err(anyhow::anyhow!("Unexpected packet capture option {id}"))?,
                        };

                        // Keep the lock until all values are being set to make the update atomic.
                        let mut pcap_writer = self.pcap.pcap_writer.lock();
                        let restart_required = start != self.pcap.enabled.load(Ordering::Relaxed);
                        self.pcap.snaplen.store(options.snaplen, Ordering::Relaxed);
                        self.pcap
                            .interface_descriptor_written
                            .store(false, Ordering::Relaxed);
                        self.pcap.enabled.store(start, Ordering::Relaxed);
                        *pcap_writer = options.writer;
                        anyhow::Ok(restart_required)
                    }
                    .await;
                    let (result, restart_required) = match result {
                        Err(e) => (Err(e), false),
                        Ok(value) => (Ok(()), value),
                    };
                    response.complete(result.map_err(RemoteError::new));
                    if restart_required {
                        break EndpointAction::RestartRequired;
                    }
                }
                Message::UpdateFromEndpoint(update) => break update,
            }
        }
    }

    fn link_speed(&self) -> u64 {
        self.current().link_speed()
    }
}

struct Pcap {
    // N.B Lock/update semantics: Keep the `pcap_writer` lock while updating
    //  the other fields.
    pcap_writer: parking_lot::Mutex<Option<Box<dyn PcapWriter>>>,
    interface_descriptor_written: AtomicBool,
    enabled: AtomicBool,
    snaplen: AtomicUsize,
    endpoint_control: mesh::Sender<PacketCaptureEndpointCommand>,
}

impl Pcap {
    fn new(endpoint_control: mesh::Sender<PacketCaptureEndpointCommand>) -> Self {
        Self {
            enabled: AtomicBool::new(false),
            snaplen: AtomicUsize::new(65535),
            pcap_writer: parking_lot::Mutex::new(None),
            interface_descriptor_written: AtomicBool::new(false),
            endpoint_control,
        }
    }

    fn write_packet(
        &self,
        buf: &[u8],
        original_len: u32,
        snaplen: u32,
        timestamp: &Duration,
    ) -> bool {
        let mut locked_writer = self.pcap_writer.lock();
        let Some(pcap_writer) = &mut *locked_writer else {
            return false;
        };

        let handle_write_result = |r: PcapResult<usize>| match r {
            // Writer gone unexpectedly; disable packet capture.
            Err(PcapError::IoError(_)) => {
                // No particular benefit of using compare_exchange atomic here
                // as the pcap writer lock is held.
                if self.enabled.load(Ordering::Relaxed) {
                    self.enabled.store(false, Ordering::Relaxed);
                    let stop = PacketCaptureOptions::new_with_stop();
                    // Best effort.
                    drop(
                        self.endpoint_control
                            .call(PacketCaptureEndpointCommand::PacketCapture, stop),
                    );
                }
                Err(())
            }
            _ => Ok(()),
        };

        if !self.interface_descriptor_written.load(Ordering::Relaxed) {
            let interface = InterfaceDescriptionBlock {
                linktype: DataLink::ETHERNET,
                snaplen,
                options: vec![],
            };
            if handle_write_result(pcap_writer.write_pcapng_block_id(interface)).is_err() {
                *locked_writer = None;
                return false;
            }
            self.interface_descriptor_written
                .store(true, Ordering::Relaxed);
        }

        let packet = EnhancedPacketBlock {
            interface_id: 0,
            timestamp: *timestamp,
            original_len,
            data: Cow::Borrowed(buf),
            options: vec![],
        };

        if handle_write_result(pcap_writer.write_pcapng_block_eb(packet)).is_err() {
            *locked_writer = None;
            return false;
        }

        true
    }
}

struct PacketCaptureQueue {
    queue: Box<dyn Queue>,
    mem: GuestMemory,
    pcap: Arc<Pcap>,
}

impl PacketCaptureQueue {
    fn current_mut(&mut self) -> &mut dyn Queue {
        self.queue.as_mut()
    }
}

/// Reads data from guest memory segments into a capture buffer, respecting
/// segment boundaries.
///
/// Returns `(bytes_copied, total_segment_len)`. The `total_segment_len` is the
/// sum of all segment lengths, including segments that extend past the end of
/// `buf` (useful for reporting the original packet length in PCAP headers).
fn read_segments_to_buf(
    mem: &GuestMemory,
    segments: impl IntoIterator<Item = (u64, u32)>,
    buf: &mut [u8],
) -> (usize, u32) {
    let mut len = 0;
    let mut total_len: u32 = 0;
    for (gpa, seg_len) in segments {
        total_len += seg_len;
        if len == buf.len() {
            continue;
        }
        let copy_length = std::cmp::min(buf.len() - len, seg_len as usize);
        let _ = mem.read_at(gpa, &mut buf[len..len + copy_length]);
        len += copy_length;
    }
    (len, total_len)
}

#[async_trait]
impl Queue for PacketCaptureQueue {
    async fn update_target_vp(&mut self, target_vp: u32) {
        self.current_mut().update_target_vp(target_vp).await
    }

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        self.current_mut().poll_ready(cx)
    }

    fn rx_avail(&mut self, done: &[RxId]) {
        self.current_mut().rx_avail(done)
    }

    fn rx_poll(&mut self, packets: &mut [RxId]) -> anyhow::Result<usize> {
        let n = self.current_mut().rx_poll(packets)?;
        if self.pcap.enabled.load(Ordering::Relaxed) {
            if let Some(pool) = self.queue.buffer_access() {
                let timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or(Duration::new(0, 0));
                let snaplen = self.pcap.snaplen.load(Ordering::Relaxed);
                for id in &packets[..n] {
                    let mut buf = vec![0; snaplen];
                    let (len, pkt_len) = read_segments_to_buf(
                        &self.mem,
                        pool.guest_addresses(*id).iter().map(|s| (s.gpa, s.len)),
                        &mut buf,
                    );

                    if len == 0 {
                        continue;
                    }

                    if !self
                        .pcap
                        .write_packet(&buf[..len], pkt_len, snaplen as u32, &timestamp)
                    {
                        break;
                    }
                }
            }
        }
        Ok(n)
    }

    fn tx_avail(&mut self, segments: &[TxSegment]) -> anyhow::Result<(bool, usize)> {
        if self.pcap.enabled.load(Ordering::Relaxed) {
            let mut segments = segments;
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::new(0, 0));
            let snaplen = self.pcap.snaplen.load(Ordering::Relaxed);
            while !segments.is_empty() {
                let (metadata, this, rest) = next_packet(segments);
                segments = rest;
                if metadata.len == 0 {
                    continue;
                }
                let mut buf = vec![0; snaplen];
                let (len, _) =
                    read_segments_to_buf(&self.mem, this.iter().map(|s| (s.gpa, s.len)), &mut buf);

                if len == 0 {
                    continue;
                }

                if !self
                    .pcap
                    .write_packet(&buf[..len], metadata.len, snaplen as u32, &timestamp)
                {
                    break;
                }
            }
        }
        self.current_mut().tx_avail(segments)
    }

    fn tx_poll(&mut self, done: &mut [TxId]) -> Result<usize, TxError> {
        self.current_mut().tx_poll(done)
    }

    fn buffer_access(&mut self) -> Option<&mut dyn BufferAccess> {
        self.queue.buffer_access()
    }
}

impl InspectMut for PacketCaptureQueue {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        self.current_mut().inspect_mut(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE_SIZE: usize = 4096;

    /// A single segment near the end of guest memory. With a snaplen larger
    /// than the segment, the old unbounded `buf[len..]` slice would cause
    /// `read_at` to attempt reading past the end of guest memory, failing
    /// silently and leaving the buffer as zeros.
    #[test]
    fn read_segments_single_segment_at_end_of_memory() {
        let mem = GuestMemory::allocate(PAGE_SIZE);
        let pattern = [0xAA_u8; 16];
        mem.write_at(PAGE_SIZE as u64 - 16, &pattern).unwrap();

        let mut buf = vec![0u8; 64];
        let (len, total_len) =
            read_segments_to_buf(&mem, [(PAGE_SIZE as u64 - 16, 16u32)], &mut buf);

        assert_eq!(len, 16);
        assert_eq!(total_len, 16);
        assert_eq!(&buf[..16], &pattern);
    }

    /// Two segments near the end of guest memory. The snaplen (64) is larger
    /// than both segments combined (32 bytes). Without the bounded slice fix,
    /// the first `read_at` would try to read 64 bytes starting at GPA 4064,
    /// crossing the end of the 4096-byte guest memory.
    #[test]
    fn read_segments_multiple_segments_at_end_of_memory() {
        let mem = GuestMemory::allocate(PAGE_SIZE);
        let pattern_a = [0xAA_u8; 16];
        let pattern_b = [0xBB_u8; 16];
        mem.write_at(PAGE_SIZE as u64 - 32, &pattern_a).unwrap();
        mem.write_at(PAGE_SIZE as u64 - 16, &pattern_b).unwrap();

        let mut buf = vec![0u8; 64];
        let (len, total_len) = read_segments_to_buf(
            &mem,
            [
                (PAGE_SIZE as u64 - 32, 16u32),
                (PAGE_SIZE as u64 - 16, 16u32),
            ],
            &mut buf,
        );

        assert_eq!(len, 32);
        assert_eq!(total_len, 32);
        assert_eq!(&buf[..16], &pattern_a);
        assert_eq!(&buf[16..32], &pattern_b);
    }

    /// Snaplen smaller than the segment length truncates the copy but still
    /// reports the full segment length as `total_len`.
    #[test]
    fn read_segments_snaplen_truncates() {
        let mem = GuestMemory::allocate(PAGE_SIZE);
        let pattern = [0xCC_u8; 64];
        mem.write_at(0, &pattern).unwrap();

        let mut buf = vec![0u8; 16];
        let (len, total_len) = read_segments_to_buf(&mem, [(0u64, 64u32)], &mut buf);

        assert_eq!(len, 16);
        assert_eq!(total_len, 64);
        assert_eq!(&buf[..16], &[0xCC; 16]);
    }

    /// Multiple segments where total data exceeds snaplen. The buffer should
    /// contain data from the first segments up to the snaplen limit, and
    /// `total_len` should reflect all segments.
    #[test]
    fn read_segments_multiple_segments_exceed_snaplen() {
        let mem = GuestMemory::allocate(PAGE_SIZE);
        mem.write_at(0, &[0xAA_u8; 32]).unwrap();
        mem.write_at(100, &[0xBB_u8; 32]).unwrap();
        mem.write_at(200, &[0xCC_u8; 32]).unwrap();

        let mut buf = vec![0u8; 48];
        let (len, total_len) = read_segments_to_buf(
            &mem,
            [(0u64, 32u32), (100u64, 32u32), (200u64, 32u32)],
            &mut buf,
        );

        assert_eq!(len, 48);
        assert_eq!(total_len, 96);
        assert_eq!(&buf[..32], &[0xAA; 32]);
        assert_eq!(&buf[32..48], &[0xBB; 16]);
    }

    /// Empty segments produce zero-length output.
    #[test]
    fn read_segments_empty() {
        let mem = GuestMemory::allocate(PAGE_SIZE);
        let mut buf = vec![0u8; 64];
        let (len, total_len) = read_segments_to_buf(&mem, std::iter::empty(), &mut buf);

        assert_eq!(len, 0);
        assert_eq!(total_len, 0);
    }
}
