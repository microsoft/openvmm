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
                    let mut len = 0;
                    let mut pkt_len = 0;
                    for segment in pool.guest_addresses(*id).iter() {
                        pkt_len += segment.len;
                        if len == buf.len() {
                            continue;
                        }

                        let copy_length = std::cmp::min(buf.len() - len, segment.len as usize);
                        let _ = self
                            .mem
                            .read_at(segment.gpa, &mut buf[len..len + copy_length]);
                        len += copy_length;
                    }

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
                let mut len = 0;
                for segment in this {
                    if len == buf.len() {
                        break;
                    }

                    let copy_length = std::cmp::min(buf.len() - len, segment.len as usize);
                    let _ = self
                        .mem
                        .read_at(segment.gpa, &mut buf[len..len + copy_length]);
                    len += copy_length;
                }

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
    use net_backend::RxBufferSegment;
    use net_backend::RxMetadata;
    use net_backend::TxMetadata;
    use net_backend::TxSegmentType;

    const PAGE_SIZE: usize = 4096;

    // -- Shared test helpers --------------------------------------------------

    /// A `Write` adapter that appends into a shared `Vec<u8>`.
    struct SharedWriter(Arc<parking_lot::Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Creates a [`Pcap`] with capture enabled and returns the raw PCAP output
    /// buffer so tests can inspect captured data.
    fn make_pcap(snaplen: usize) -> (Arc<Pcap>, Arc<parking_lot::Mutex<Vec<u8>>>) {
        let (control_tx, _control_rx) = mesh::channel();
        let output = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let pcap = Arc::new(Pcap::new(control_tx));
        pcap.enabled.store(true, Ordering::Relaxed);
        pcap.snaplen.store(snaplen, Ordering::Relaxed);

        let pcap_ng_writer =
            PcapNgWriter::with_endianness(SharedWriter(output.clone()), pcap_file::Endianness::Big)
                .unwrap();
        *pcap.pcap_writer.lock() = Some(Box::new(LocalPcapWriter {
            inner: pcap_ng_writer,
        }));

        (pcap, output)
    }

    // -- Mock BufferAccess / Queue for RX tests --------------------------------

    /// A [`BufferAccess`] that maps every [`RxId`] to a fixed set of guest
    /// memory segments, allowing the test to control exactly where in guest
    /// memory the capture code reads.
    struct MockBufferAccess {
        mem: GuestMemory,
        segments: Vec<RxBufferSegment>,
    }

    impl BufferAccess for MockBufferAccess {
        fn guest_memory(&self) -> &GuestMemory {
            &self.mem
        }
        fn guest_addresses(&mut self, _id: RxId) -> &[RxBufferSegment] {
            &self.segments
        }
        fn capacity(&self, _id: RxId) -> u32 {
            self.segments.iter().map(|s| s.len).sum()
        }
        fn write_data(&mut self, _id: RxId, _buf: &[u8]) {}
        fn write_header(&mut self, _id: RxId, _metadata: &RxMetadata) {}
    }

    /// A mock inner [`Queue`] for RX‐capture tests.
    ///
    /// * `rx_poll` returns the pre‐loaded [`RxId`]s.
    /// * `buffer_access` exposes the controlled [`MockBufferAccess`].
    #[derive(InspectMut)]
    #[inspect(skip)]
    struct MockRxQueue {
        rx_packets: Vec<RxId>,
        pool: MockBufferAccess,
    }

    impl Queue for MockRxQueue {
        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<()> {
            Poll::Ready(())
        }
        fn rx_avail(&mut self, _done: &[RxId]) {}
        fn rx_poll(&mut self, packets: &mut [RxId]) -> anyhow::Result<usize> {
            let n = packets.len().min(self.rx_packets.len());
            for (d, s) in packets.iter_mut().zip(self.rx_packets.drain(..n)) {
                *d = s;
            }
            Ok(n)
        }
        fn tx_avail(&mut self, _segments: &[TxSegment]) -> anyhow::Result<(bool, usize)> {
            Ok((false, 0))
        }
        fn tx_poll(&mut self, _done: &mut [TxId]) -> Result<usize, TxError> {
            Ok(0)
        }
        fn buffer_access(&mut self) -> Option<&mut dyn BufferAccess> {
            Some(&mut self.pool)
        }
    }

    // -- Mock Queue for TX tests -----------------------------------------------

    /// A mock inner [`Queue`] for TX‐capture tests.  `tx_avail` accepts every
    /// segment without doing any real work.
    #[derive(InspectMut)]
    #[inspect(skip)]
    struct MockTxQueue;

    impl Queue for MockTxQueue {
        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<()> {
            Poll::Ready(())
        }
        fn rx_avail(&mut self, _done: &[RxId]) {}
        fn rx_poll(&mut self, _packets: &mut [RxId]) -> anyhow::Result<usize> {
            Ok(0)
        }
        fn tx_avail(&mut self, segments: &[TxSegment]) -> anyhow::Result<(bool, usize)> {
            Ok((false, segments.len()))
        }
        fn tx_poll(&mut self, _done: &mut [TxId]) -> Result<usize, TxError> {
            Ok(0)
        }
        fn buffer_access(&mut self) -> Option<&mut dyn BufferAccess> {
            None
        }
    }

    // -- RX tests --------------------------------------------------------------

    /// Exercises `PacketCaptureQueue::rx_poll` with a single segment positioned
    /// at the very end of guest memory.  The snaplen (65535) is much larger
    /// than the 16‐byte segment.
    ///
    /// Before the fix the capture code passed `&mut buf[len..]` (65535 bytes)
    /// to `read_at`, causing it to attempt reading 65535 bytes starting at
    /// GPA 4080 — well past the 4096‐byte memory.  The read failed, the error
    /// was silently swallowed (`let _ = …`), and the capture buffer stayed
    /// zero‐filled.  After the fix, only `copy_length` (16) bytes are
    /// requested and the read succeeds.
    #[test]
    fn rx_poll_captures_segment_at_end_of_memory() {
        let mem = GuestMemory::allocate(PAGE_SIZE);
        let pattern = [0xAA_u8; 16];
        let gpa = PAGE_SIZE as u64 - 16;
        mem.write_at(gpa, &pattern).unwrap();

        let mock_queue = MockRxQueue {
            rx_packets: vec![RxId(0)],
            pool: MockBufferAccess {
                mem: mem.clone(),
                segments: vec![RxBufferSegment { gpa, len: 16 }],
            },
        };

        let (pcap, output) = make_pcap(65535);
        let mut queue = PacketCaptureQueue {
            queue: Box::new(mock_queue),
            mem,
            pcap,
        };

        let mut packets = [RxId(0); 1];
        let n = queue.rx_poll(&mut packets).unwrap();
        assert_eq!(n, 1);

        // The PCAP output must contain the 0xAA pattern.  Before the fix the
        // buffer was all zeros, so this assertion would fail.
        let output = output.lock();
        assert!(
            output.windows(16).any(|w| w == pattern),
            "PCAP output should contain the 0xAA pattern from guest memory, \
             got all zeros (read_at over-read failed silently)"
        );
    }

    /// Like the single‐segment test but with *two* segments near the end of
    /// guest memory.  The first segment starts 32 bytes before the end, the
    /// second 16 bytes before the end.
    ///
    /// With the old unbounded slice the first `read_at` already tries to read
    /// 65535 bytes from GPA 4064, which fails.
    #[test]
    fn rx_poll_captures_multiple_segments_at_end_of_memory() {
        let mem = GuestMemory::allocate(PAGE_SIZE);
        let pattern_a = [0xAA_u8; 16];
        let pattern_b = [0xBB_u8; 16];
        let gpa_a = PAGE_SIZE as u64 - 32;
        let gpa_b = PAGE_SIZE as u64 - 16;
        mem.write_at(gpa_a, &pattern_a).unwrap();
        mem.write_at(gpa_b, &pattern_b).unwrap();

        let mock_queue = MockRxQueue {
            rx_packets: vec![RxId(0)],
            pool: MockBufferAccess {
                mem: mem.clone(),
                segments: vec![
                    RxBufferSegment {
                        gpa: gpa_a,
                        len: 16,
                    },
                    RxBufferSegment {
                        gpa: gpa_b,
                        len: 16,
                    },
                ],
            },
        };

        let (pcap, output) = make_pcap(65535);
        let mut queue = PacketCaptureQueue {
            queue: Box::new(mock_queue),
            mem,
            pcap,
        };

        let mut packets = [RxId(0); 1];
        let n = queue.rx_poll(&mut packets).unwrap();
        assert_eq!(n, 1);

        let output = output.lock();
        assert!(
            output.windows(16).any(|w| w == pattern_a),
            "PCAP output should contain the 0xAA pattern (segment 1)"
        );
        assert!(
            output.windows(16).any(|w| w == pattern_b),
            "PCAP output should contain the 0xBB pattern (segment 2)"
        );
    }

    // -- TX tests --------------------------------------------------------------

    /// Exercises `PacketCaptureQueue::tx_avail` with a single TX segment at
    /// the end of guest memory.  Same root cause as the RX test: the old code
    /// passed the full remaining buffer to `read_at`.
    #[test]
    fn tx_avail_captures_segment_at_end_of_memory() {
        let mem = GuestMemory::allocate(PAGE_SIZE);
        let pattern = [0xBB_u8; 16];
        let gpa = PAGE_SIZE as u64 - 16;
        mem.write_at(gpa, &pattern).unwrap();

        let (pcap, output) = make_pcap(65535);
        let mut queue = PacketCaptureQueue {
            queue: Box::new(MockTxQueue),
            mem,
            pcap,
        };

        let segments = vec![TxSegment {
            ty: TxSegmentType::Head(TxMetadata {
                id: TxId(0),
                segment_count: 1,
                len: 16,
                ..Default::default()
            }),
            gpa,
            len: 16,
        }];

        let _ = queue.tx_avail(&segments).unwrap();

        let output = output.lock();
        assert!(
            output.windows(16).any(|w| w == pattern),
            "PCAP output should contain the 0xBB pattern from guest memory, \
             got all zeros (read_at over-read failed silently)"
        );
    }

    /// Exercises `PacketCaptureQueue::tx_avail` with two TX segments (head +
    /// tail) near the end of guest memory.
    #[test]
    fn tx_avail_captures_multiple_segments_at_end_of_memory() {
        let mem = GuestMemory::allocate(PAGE_SIZE);
        let pattern_a = [0xCC_u8; 16];
        let pattern_b = [0xDD_u8; 16];
        let gpa_a = PAGE_SIZE as u64 - 32;
        let gpa_b = PAGE_SIZE as u64 - 16;
        mem.write_at(gpa_a, &pattern_a).unwrap();
        mem.write_at(gpa_b, &pattern_b).unwrap();

        let (pcap, output) = make_pcap(65535);
        let mut queue = PacketCaptureQueue {
            queue: Box::new(MockTxQueue),
            mem,
            pcap,
        };

        let segments = vec![
            TxSegment {
                ty: TxSegmentType::Head(TxMetadata {
                    id: TxId(0),
                    segment_count: 2,
                    len: 32,
                    ..Default::default()
                }),
                gpa: gpa_a,
                len: 16,
            },
            TxSegment {
                ty: TxSegmentType::Tail,
                gpa: gpa_b,
                len: 16,
            },
        ];

        let _ = queue.tx_avail(&segments).unwrap();

        let output = output.lock();
        assert!(
            output.windows(16).any(|w| w == pattern_a),
            "PCAP output should contain the 0xCC pattern (segment 1)"
        );
        assert!(
            output.windows(16).any(|w| w == pattern_b),
            "PCAP output should contain the 0xDD pattern (segment 2)"
        );
    }
}
