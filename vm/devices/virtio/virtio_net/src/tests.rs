// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use async_trait::async_trait;
use guestmem::GuestMemory;
use inspect::InspectMut;
use net_backend::Endpoint;
use net_backend::EndpointAction;
use net_backend::MultiQueueSupport;
use net_backend::QueueConfig;
use net_backend::RssConfig;
use net_backend::RxId;
use net_backend::RxMetadata;
use net_backend::TxError;
use net_backend::TxId;
use net_backend::TxOffloadSupport;
use net_backend::TxSegment;
use net_backend::TxSegmentType;
use net_backend_resources::mac_address::MacAddress;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::wait::PolledWait;
use pal_event::Event;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::future::pending;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;
use test_with_tracing::test;
use virtio::QueueResources;
use virtio::Resources;
use virtio::VirtioDevice;
use virtio::queue::QueueParams;
use virtio::spec::VirtioDeviceFeatures;
use virtio::spec::queue::DescriptorFlags;
use vmcore::interrupt::Interrupt;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;

use crate::Device;

// --- Constants ---

const QUEUE_SIZE: u16 = 16;

// Memory layout for queue 0 (RX)
const RX_DESC_ADDR: u64 = 0x0000;
const RX_AVAIL_ADDR: u64 = 0x1000;
const RX_USED_ADDR: u64 = 0x2000;

// Memory layout for queue 1 (TX)
const TX_DESC_ADDR: u64 = 0x10000;
const TX_AVAIL_ADDR: u64 = 0x11000;
const TX_USED_ADDR: u64 = 0x12000;

// Data area for TX packet headers and payloads
const DATA_BASE: u64 = 0x20000;
const TOTAL_MEM_SIZE: usize = 0x30000;

// Virtio-net header size, derived from the actual layout.
const NET_HEADER_SIZE: u32 = crate::header_size() as u32;

// --- Simplified segment info for assertions ---

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct TxSegmentInfo {
    is_head: bool,
    tx_id: Option<u32>,
    gpa: u64,
    len: u32,
}

// --- TxAvailBehavior ---

struct TxAvailBehavior {
    sync: bool,
    consume_all: bool,
    consume_count: Option<usize>,
}

impl Default for TxAvailBehavior {
    fn default() -> Self {
        Self {
            sync: true,
            consume_all: true,
            consume_count: None,
        }
    }
}

// --- MockQueue ---

struct MockQueue {
    tx_avail_behavior: Arc<Mutex<TxAvailBehavior>>,
    tx_avail_log: Arc<Mutex<Vec<Vec<TxSegmentInfo>>>>,
    tx_completions: Arc<Mutex<VecDeque<Vec<TxId>>>>,
    rx_pending: Arc<Mutex<VecDeque<RxId>>>,
    rx_ready: Arc<Mutex<VecDeque<RxId>>>,
    #[allow(dead_code)] // kept alive for the MockQueueHandle's Arc clone
    pool: Arc<Mutex<Option<Box<dyn net_backend::BufferAccess>>>>,
    ready_waker: Arc<Mutex<Option<Waker>>>,
    rx_avail_notify: mesh::Sender<()>,
    tx_avail_notify: mesh::Sender<()>,
}

impl InspectMut for MockQueue {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.ignore();
    }
}

#[async_trait]
impl net_backend::Queue for MockQueue {
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let completions = self.tx_completions.lock();
        if !completions.is_empty() {
            return Poll::Ready(());
        }
        drop(completions);
        let rx_ready = self.rx_ready.lock();
        if !rx_ready.is_empty() {
            return Poll::Ready(());
        }
        drop(rx_ready);
        *self.ready_waker.lock() = Some(cx.waker().clone());
        Poll::Pending
    }

    fn rx_avail(&mut self, done: &[RxId]) {
        self.rx_pending.lock().extend(done.iter().copied());
        for _ in done {
            self.rx_avail_notify.send(());
        }
    }

    fn rx_poll(&mut self, packets: &mut [RxId]) -> anyhow::Result<usize> {
        let mut ready = self.rx_ready.lock();
        let n = ready.len().min(packets.len());
        for packet in packets.iter_mut().take(n) {
            *packet = ready.pop_front().unwrap();
        }
        Ok(n)
    }

    fn tx_avail(&mut self, segments: &[TxSegment]) -> anyhow::Result<(bool, usize)> {
        // Log the segments
        let infos: Vec<TxSegmentInfo> = segments
            .iter()
            .map(|seg| {
                let (is_head, tx_id) = match &seg.ty {
                    TxSegmentType::Head(meta) => (true, Some(meta.id.0)),
                    TxSegmentType::Tail => (false, None),
                };
                TxSegmentInfo {
                    is_head,
                    tx_id,
                    gpa: seg.gpa,
                    len: seg.len,
                }
            })
            .collect();
        self.tx_avail_log.lock().push(infos);
        self.tx_avail_notify.send(());

        let behavior = self.tx_avail_behavior.lock();
        let consumed = if behavior.consume_all {
            segments.len()
        } else if let Some(count) = behavior.consume_count {
            count.min(segments.len())
        } else {
            segments.len()
        };
        Ok((behavior.sync, consumed))
    }

    fn tx_poll(&mut self, done: &mut [TxId]) -> Result<usize, TxError> {
        let mut completions = self.tx_completions.lock();
        if let Some(batch) = completions.pop_front() {
            let n = batch.len().min(done.len());
            done[..n].copy_from_slice(&batch[..n]);
            Ok(n)
        } else {
            Ok(0)
        }
    }

    fn buffer_access(&mut self) -> Option<&mut dyn net_backend::BufferAccess> {
        None
    }
}

// --- MockQueueHandle ---

struct MockQueueHandle {
    tx_avail_behavior: Arc<Mutex<TxAvailBehavior>>,
    tx_avail_log: Arc<Mutex<Vec<Vec<TxSegmentInfo>>>>,
    tx_completions: Arc<Mutex<VecDeque<Vec<TxId>>>>,
    rx_pending: Arc<Mutex<VecDeque<RxId>>>,
    rx_ready: Arc<Mutex<VecDeque<RxId>>>,
    pool: Arc<Mutex<Option<Box<dyn net_backend::BufferAccess>>>>,
    ready_waker: Arc<Mutex<Option<Waker>>>,
    rx_avail_notify: mesh::Receiver<()>,
    tx_avail_notify: mesh::Receiver<()>,
}

impl MockQueueHandle {
    fn complete_tx(&self, ids: Vec<TxId>) {
        self.tx_completions.lock().push_back(ids);
        if let Some(waker) = self.ready_waker.lock().take() {
            waker.wake();
        }
    }

    fn take_tx_avail_log(&self) -> Vec<Vec<TxSegmentInfo>> {
        std::mem::take(&mut *self.tx_avail_log.lock())
    }

    /// Inject an RX packet into a pending RX buffer.
    ///
    /// Takes a buffer from the pending queue (posted by the guest via
    /// `rx_avail`), writes the packet data into it via `BufferAccess`,
    /// and makes it available for `rx_poll`. Wakes the backend so the
    /// device processes the completion.
    fn inject_rx_packet(&self, data: &[u8]) {
        let rx_id = self
            .rx_pending
            .lock()
            .pop_front()
            .expect("no pending RX buffer available");
        let mut pool_guard = self.pool.lock();
        let pool = pool_guard.as_mut().expect("pool not set");
        let metadata = RxMetadata {
            offset: 0,
            len: data.len(),
            ..Default::default()
        };
        pool.write_packet(rx_id, &metadata, data);
        drop(pool_guard);
        self.rx_ready.lock().push_back(rx_id);
        if let Some(waker) = self.ready_waker.lock().take() {
            waker.wake();
        }
    }

    /// Wait until the device has called `rx_avail` for at least one buffer.
    async fn wait_for_rx_pending(&mut self) {
        mesh::CancelContext::new()
            .with_timeout(Duration::from_secs(5))
            .until_cancelled(self.rx_avail_notify.next())
            .await
            .expect("timed out waiting for rx_avail")
            .expect("channel closed");
    }

    /// Wait until the device has called `tx_avail` at least once.
    async fn wait_for_tx_avail(&mut self) {
        mesh::CancelContext::new()
            .with_timeout(Duration::from_secs(5))
            .until_cancelled(self.tx_avail_notify.next())
            .await
            .expect("timed out waiting for tx_avail")
            .expect("channel closed");
    }
}

fn new_mock_queue(pool: Box<dyn net_backend::BufferAccess>) -> (MockQueue, MockQueueHandle) {
    let tx_avail_behavior = Arc::new(Mutex::new(TxAvailBehavior::default()));
    let tx_avail_log = Arc::new(Mutex::new(Vec::new()));
    let tx_completions = Arc::new(Mutex::new(VecDeque::new()));
    let rx_pending = Arc::new(Mutex::new(VecDeque::new()));
    let rx_ready = Arc::new(Mutex::new(VecDeque::new()));
    let pool = Arc::new(Mutex::new(Some(pool)));
    let ready_waker = Arc::new(Mutex::new(None));
    let (rx_avail_tx, rx_avail_rx) = mesh::channel();
    let (tx_avail_tx, tx_avail_rx) = mesh::channel();

    let queue = MockQueue {
        tx_avail_behavior: tx_avail_behavior.clone(),
        tx_avail_log: tx_avail_log.clone(),
        tx_completions: tx_completions.clone(),
        rx_pending: rx_pending.clone(),
        rx_ready: rx_ready.clone(),
        pool: pool.clone(),
        ready_waker: ready_waker.clone(),
        rx_avail_notify: rx_avail_tx,
        tx_avail_notify: tx_avail_tx,
    };
    let handle = MockQueueHandle {
        tx_avail_behavior,
        tx_avail_log,
        tx_completions,
        rx_pending,
        rx_ready,
        pool,
        ready_waker,
        rx_avail_notify: rx_avail_rx,
        tx_avail_notify: tx_avail_rx,
    };
    (queue, handle)
}

// --- MockEndpoint ---

struct MockEndpoint {
    queue_tx: mesh::Sender<MockQueueHandle>,
}

impl InspectMut for MockEndpoint {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.ignore();
    }
}

#[async_trait]
impl Endpoint for MockEndpoint {
    fn endpoint_type(&self) -> &'static str {
        "mock"
    }

    async fn get_queues(
        &mut self,
        config: Vec<QueueConfig<'_>>,
        _rss: Option<&RssConfig<'_>>,
        queues: &mut Vec<Box<dyn net_backend::Queue>>,
    ) -> anyhow::Result<()> {
        let pool = config.into_iter().next().unwrap().pool;
        let (queue, handle) = new_mock_queue(pool);
        self.queue_tx.send(handle);
        queues.push(Box::new(queue));
        Ok(())
    }

    async fn stop(&mut self) {}

    fn is_ordered(&self) -> bool {
        false
    }

    fn tx_offload_support(&self) -> TxOffloadSupport {
        TxOffloadSupport::default()
    }

    fn multiqueue_support(&self) -> MultiQueueSupport {
        MultiQueueSupport {
            max_queues: 1,
            indirection_table_size: 0,
        }
    }

    fn tx_fast_completions(&self) -> bool {
        true
    }

    async fn wait_for_endpoint_action(&mut self) -> EndpointAction {
        pending().await
    }
}

// --- Guest memory helpers ---

/// Write a split virtio descriptor at the given descriptor table base.
fn write_descriptor(
    mem: &GuestMemory,
    desc_table_base: u64,
    index: u16,
    addr: u64,
    len: u32,
    flags: DescriptorFlags,
    next: u16,
) {
    let base = desc_table_base + 16 * index as u64;
    mem.write_at(base, &addr.to_le_bytes()).unwrap();
    mem.write_at(base + 8, &len.to_le_bytes()).unwrap();
    mem.write_at(base + 12, &u16::from(flags).to_le_bytes())
        .unwrap();
    mem.write_at(base + 14, &next.to_le_bytes()).unwrap();
}

/// Initialize avail ring (flags=0, idx=0).
fn init_avail_ring(mem: &GuestMemory, avail_addr: u64) {
    mem.write_at(avail_addr, &0u16.to_le_bytes()).unwrap(); // flags
    mem.write_at(avail_addr + 2, &0u16.to_le_bytes()).unwrap(); // idx
}

/// Initialize used ring (flags=0, idx=0).
fn init_used_ring(mem: &GuestMemory, used_addr: u64) {
    mem.write_at(used_addr, &0u16.to_le_bytes()).unwrap(); // flags
    mem.write_at(used_addr + 2, &0u16.to_le_bytes()).unwrap(); // idx
}

/// Post a TX packet as a descriptor chain.
///
/// The first descriptor covers the virtio-net header. Subsequent descriptors
/// cover data segments. All descriptors are chained via the `next` field.
fn post_tx_packet(
    mem: &GuestMemory,
    head_desc_index: u16,
    header_gpa: u64,
    header_len: u32,
    data_segments: &[(u64, u32)],
) {
    // Write header descriptor
    let has_next = !data_segments.is_empty();
    let header_flags = DescriptorFlags::new().with_next(has_next);
    let next_idx = if has_next { head_desc_index + 1 } else { 0 };
    write_descriptor(
        mem,
        TX_DESC_ADDR,
        head_desc_index,
        header_gpa,
        header_len,
        header_flags,
        next_idx,
    );

    // Write data descriptors
    for (i, &(gpa, len)) in data_segments.iter().enumerate() {
        let desc_idx = head_desc_index + 1 + i as u16;
        let is_last = i == data_segments.len() - 1;
        let flags = DescriptorFlags::new().with_next(!is_last);
        let next = if is_last { 0 } else { desc_idx + 1 };
        write_descriptor(mem, TX_DESC_ADDR, desc_idx, gpa, len, flags, next);
    }
}

/// Make a descriptor index available in the avail ring and bump the index.
fn make_available(mem: &GuestMemory, avail_addr: u64, desc_index: u16, avail_idx: &mut u16) {
    let ring_offset = avail_addr + 4 + 2 * (*avail_idx % QUEUE_SIZE) as u64;
    mem.write_at(ring_offset, &desc_index.to_le_bytes())
        .unwrap();
    *avail_idx = avail_idx.wrapping_add(1);
    // Write the new avail idx
    mem.write_at(avail_addr + 2, &avail_idx.to_le_bytes())
        .unwrap();
}

/// Read the used ring index.
fn read_used_idx(mem: &GuestMemory, used_addr: u64) -> u16 {
    let mut buf = [0u8; 2];
    mem.read_at(used_addr + 2, &mut buf).unwrap();
    u16::from_le_bytes(buf)
}

/// Read a used ring entry.
fn read_used_entry(mem: &GuestMemory, used_addr: u64, index: u16) -> (u32, u32) {
    let entry_offset = used_addr + 4 + 8 * (index % QUEUE_SIZE) as u64;
    let mut id_buf = [0u8; 4];
    let mut len_buf = [0u8; 4];
    mem.read_at(entry_offset, &mut id_buf).unwrap();
    mem.read_at(entry_offset + 4, &mut len_buf).unwrap();
    (u32::from_le_bytes(id_buf), u32::from_le_bytes(len_buf))
}

/// Read the next TX used ring entry, returning (desc_id, bytes_written) or None.
fn read_used(mem: &GuestMemory, used_idx: &mut u16) -> Option<(u16, u32)> {
    let current_used_idx = read_used_idx(mem, TX_USED_ADDR);
    if current_used_idx == *used_idx {
        return None;
    }
    let (id, len) = read_used_entry(mem, TX_USED_ADDR, *used_idx);
    *used_idx = used_idx.wrapping_add(1);
    Some((id as u16, len))
}

/// Read the next RX used ring entry, returning (desc_id, bytes_written) or None.
fn read_rx_used(mem: &GuestMemory, used_idx: &mut u16) -> Option<(u16, u32)> {
    let current_used_idx = read_used_idx(mem, RX_USED_ADDR);
    if current_used_idx == *used_idx {
        return None;
    }
    let (id, len) = read_used_entry(mem, RX_USED_ADDR, *used_idx);
    *used_idx = used_idx.wrapping_add(1);
    Some((id as u16, len))
}

// --- Test Harness ---

struct TestHarness {
    device: Device,
    mem: GuestMemory,
    driver: DefaultDriver,
    queue_handle_rx: mesh::Receiver<MockQueueHandle>,
    rx_event: Event,
    rx_interrupt_event: Event,
    tx_event: Event,
    tx_interrupt_event: Event,
    rx_avail_idx: u16,
    rx_used_idx: u16,
    tx_avail_idx: u16,
    tx_used_idx: u16,
    next_data_offset: u64,
}

impl TestHarness {
    fn new(driver: &DefaultDriver) -> Self {
        let mem = GuestMemory::allocate(TOTAL_MEM_SIZE);

        // Initialize RX queue rings
        init_avail_ring(&mem, RX_AVAIL_ADDR);
        init_used_ring(&mem, RX_USED_ADDR);

        // Initialize TX queue rings
        init_avail_ring(&mem, TX_AVAIL_ADDR);
        init_used_ring(&mem, TX_USED_ADDR);

        // Create mock endpoint with channel
        let (queue_tx, queue_handle_rx) = mesh::channel();
        let endpoint = MockEndpoint { queue_tx };

        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
        let mac = MacAddress::new([0x00, 0x15, 0x5d, 0xaa, 0xbb, 0xcc]);
        let device = Device::builder().build(&driver_source, mem.clone(), Box::new(endpoint), mac);

        let rx_event = Event::new();
        let rx_interrupt_event = Event::new();
        let tx_event = Event::new();
        let tx_interrupt_event = Event::new();

        Self {
            device,
            mem,
            driver: driver.clone(),
            queue_handle_rx,
            rx_event,
            rx_interrupt_event,
            tx_event,
            tx_interrupt_event,
            rx_avail_idx: 0,
            rx_used_idx: 0,
            tx_avail_idx: 0,
            tx_used_idx: 0,
            next_data_offset: DATA_BASE,
        }
    }

    /// Enable the device and retrieve the MockQueueHandle.
    async fn enable_and_get_handle(&mut self) -> MockQueueHandle {
        let rx_interrupt = Interrupt::from_event(self.rx_interrupt_event.clone());
        let tx_interrupt = Interrupt::from_event(self.tx_interrupt_event.clone());

        let resources = Resources {
            features: VirtioDeviceFeatures::new(),
            queues: vec![
                // Queue 0: RX
                QueueResources {
                    params: QueueParams {
                        size: QUEUE_SIZE,
                        enable: true,
                        desc_addr: RX_DESC_ADDR,
                        avail_addr: RX_AVAIL_ADDR,
                        used_addr: RX_USED_ADDR,
                    },
                    notify: rx_interrupt,
                    event: self.rx_event.clone(),
                },
                // Queue 1: TX
                QueueResources {
                    params: QueueParams {
                        size: QUEUE_SIZE,
                        enable: true,
                        desc_addr: TX_DESC_ADDR,
                        avail_addr: TX_AVAIL_ADDR,
                        used_addr: TX_USED_ADDR,
                    },
                    notify: tx_interrupt,
                    event: self.tx_event.clone(),
                },
            ],
            shared_memory_region: None,
            shared_memory_size: 0,
        };

        self.device.enable(resources).unwrap();

        // Wait for the mock endpoint to provide a queue handle
        mesh::CancelContext::new()
            .with_timeout(Duration::from_secs(5))
            .until_cancelled(self.queue_handle_rx.next())
            .await
            .expect("timed out waiting for mock queue handle")
            .expect("channel closed")
    }

    /// Allocate a data region in guest memory and return its GPA.
    fn alloc_data(&mut self, size: u32) -> u64 {
        let gpa = self.next_data_offset;
        self.next_data_offset += size as u64;
        assert!(
            self.next_data_offset <= TOTAL_MEM_SIZE as u64,
            "ran out of test memory"
        );
        gpa
    }

    /// Post a TX packet with a header + one data segment, make it available, and signal.
    fn post_tx_and_signal(&mut self, desc_index: u16, data_len: u32) {
        let header_gpa = self.alloc_data(NET_HEADER_SIZE);
        let data_gpa = self.alloc_data(data_len);

        // Write a zero virtio-net header
        let header_bytes = vec![0u8; NET_HEADER_SIZE as usize];
        self.mem.write_at(header_gpa, &header_bytes).unwrap();

        // Write some data
        let data_bytes = vec![0xABu8; data_len as usize];
        self.mem.write_at(data_gpa, &data_bytes).unwrap();

        post_tx_packet(
            &self.mem,
            desc_index,
            header_gpa,
            NET_HEADER_SIZE,
            &[(data_gpa, data_len)],
        );

        make_available(&self.mem, TX_AVAIL_ADDR, desc_index, &mut self.tx_avail_idx);
        self.tx_event.signal();
    }

    /// Wait for the next TX used ring entry with a timeout.
    async fn wait_for_used(&mut self) -> (u16, u32) {
        let mut wait = PolledWait::new(&self.driver, self.tx_interrupt_event.clone()).unwrap();
        mesh::CancelContext::new()
            .with_timeout(Duration::from_secs(5))
            .until_cancelled(async {
                loop {
                    if let Some(entry) = read_used(&self.mem, &mut self.tx_used_idx) {
                        return entry;
                    }
                    wait.wait().await.unwrap();
                }
            })
            .await
            .expect("timed out waiting for TX used ring entry")
    }

    // --- RX helpers ---

    /// Post a single writeable RX buffer descriptor, make it available, and
    /// signal the RX queue so the device picks it up.
    fn post_rx_buffer_and_signal(&mut self, desc_index: u16, buffer_size: u32) -> u64 {
        let gpa = self.alloc_data(buffer_size);
        // Zero the buffer so we can detect writes
        let zeroes = vec![0u8; buffer_size as usize];
        self.mem.write_at(gpa, &zeroes).unwrap();

        // Write a single writeable descriptor (WRITE flag set)
        let flags = DescriptorFlags::new().with_write(true);
        write_descriptor(
            &self.mem,
            RX_DESC_ADDR,
            desc_index,
            gpa,
            buffer_size,
            flags,
            0,
        );
        make_available(&self.mem, RX_AVAIL_ADDR, desc_index, &mut self.rx_avail_idx);
        self.rx_event.signal();
        gpa
    }

    /// Wait for the next RX used ring entry with a timeout.
    async fn wait_for_rx_used(&mut self) -> (u16, u32) {
        let mut wait = PolledWait::new(&self.driver, self.rx_interrupt_event.clone()).unwrap();
        mesh::CancelContext::new()
            .with_timeout(Duration::from_secs(5))
            .until_cancelled(async {
                loop {
                    if let Some(entry) = read_rx_used(&self.mem, &mut self.rx_used_idx) {
                        return entry;
                    }
                    wait.wait().await.unwrap();
                }
            })
            .await
            .expect("timed out waiting for RX used ring entry")
    }

    /// Disable the device (calls poll_disable to completion).
    async fn disable(&mut self) {
        futures::future::poll_fn(|cx| self.device.poll_disable(cx)).await;
    }

    /// Reset memory layout tracking for a fresh enable cycle.
    fn reset_rings(&mut self) {
        init_avail_ring(&self.mem, RX_AVAIL_ADDR);
        init_used_ring(&self.mem, RX_USED_ADDR);
        init_avail_ring(&self.mem, TX_AVAIL_ADDR);
        init_used_ring(&self.mem, TX_USED_ADDR);
        self.rx_avail_idx = 0;
        self.rx_used_idx = 0;
        self.tx_avail_idx = 0;
        self.tx_used_idx = 0;
        self.next_data_offset = DATA_BASE;
    }
}

// --- Tests ---

use futures::StreamExt;

/// Post 1 TX packet. tx_avail returns (sync: true, all segments consumed).
/// Verify segments logged and used ring updated.
#[async_test]
async fn sync_single_packet(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    let handle = harness.enable_and_get_handle().await;

    // Default behavior: sync=true, consume_all=true
    let desc_index: u16 = 0;
    let data_len: u32 = 100;
    harness.post_tx_and_signal(desc_index, data_len);

    // Wait for the packet to appear in the used ring
    let (used_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_id, desc_index);
    assert_eq!(used_len, 0); // complete(0) writes 0 bytes

    // Verify tx_avail_log
    let log = handle.take_tx_avail_log();
    assert_eq!(log.len(), 1, "expected exactly 1 tx_avail call");
    let segments = &log[0];
    // Should have 1 segment: Head with the data (header bytes stripped)
    assert_eq!(segments.len(), 1, "expected 1 segment (data after header)");
    assert!(segments[0].is_head, "first segment should be Head");
    assert_eq!(segments[0].tx_id, Some(desc_index as u32));
    assert_eq!(segments[0].len, data_len);
}

/// Post 1 TX packet. tx_avail returns (sync: false, all segments consumed).
/// Then inject completion via handle.complete_tx(). Verify used ring updated.
#[async_test]
async fn async_completion_single_packet(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    let mut handle = harness.enable_and_get_handle().await;

    // Configure async behavior
    {
        let mut behavior = handle.tx_avail_behavior.lock();
        behavior.sync = false;
    }

    let desc_index: u16 = 0;
    let data_len: u32 = 64;
    harness.post_tx_and_signal(desc_index, data_len);

    // Wait for the device to process the TX and call tx_avail
    handle.wait_for_tx_avail().await;

    // Verify tx_avail was called
    let log = handle.take_tx_avail_log();
    assert_eq!(log.len(), 1, "expected exactly 1 tx_avail call");
    let segments = &log[0];
    assert_eq!(segments.len(), 1);
    assert!(segments[0].is_head);
    assert_eq!(segments[0].tx_id, Some(desc_index as u32));

    // Used ring should be empty since it's async
    assert!(
        read_used(&harness.mem, &mut harness.tx_used_idx).is_none(),
        "used ring should be empty before async completion"
    );

    // Inject the completion
    handle.complete_tx(vec![TxId(desc_index as u32)]);

    // Now wait for it to appear in used ring
    let (used_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_id, desc_index);
    assert_eq!(used_len, 0);
}

/// Post 3 TX packets one at a time, each completing synchronously.
/// Verify all 3 appear in used ring in order.
#[async_test]
async fn three_sequential_sync_packets(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    let handle = harness.enable_and_get_handle().await;

    // Default behavior: sync=true, consume_all=true
    // Each packet uses 2 descriptors (header + 1 data), so space them out:
    // Packet 0: desc 0,1
    // Packet 1: desc 2,3
    // Packet 2: desc 4,5
    for i in 0u16..3 {
        let desc_index = i * 2;
        let data_len = 50 + i as u32 * 10;
        harness.post_tx_and_signal(desc_index, data_len);

        let (used_id, used_len) = harness.wait_for_used().await;
        assert_eq!(used_id, desc_index, "packet {} used id mismatch", i);
        assert_eq!(used_len, 0);
    }

    // Verify tx_avail_log has 3 calls
    let log = handle.take_tx_avail_log();
    assert_eq!(log.len(), 3, "expected 3 tx_avail calls");
    for (i, segments) in log.iter().enumerate() {
        assert_eq!(segments.len(), 1, "packet {} should have 1 segment", i);
        let expected_id = (i as u16 * 2) as u32;
        assert!(segments[0].is_head);
        assert_eq!(segments[0].tx_id, Some(expected_id));
    }
}

/// Partial submit with multiple packets — Phase 2 contract test.
#[ignore = "requires partial-consume backend support not yet implemented"]
#[async_test]
async fn partial_submit_multi_packet(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    let handle = harness.enable_and_get_handle().await;

    // Configure: consume only 1 segment (first packet has 1 data segment)
    {
        let mut behavior = handle.tx_avail_behavior.lock();
        behavior.sync = true;
        behavior.consume_all = false;
        behavior.consume_count = Some(1); // consume first packet's segments only
    }

    // Post 3 packets to avail ring simultaneously before signaling
    // Packet 0: desc 0,1
    // Packet 1: desc 2,3
    // Packet 2: desc 4,5
    for i in 0u16..3 {
        let desc_index = i * 2;
        let header_gpa = harness.alloc_data(NET_HEADER_SIZE);
        let data_gpa = harness.alloc_data(64);
        let header_bytes = vec![0u8; NET_HEADER_SIZE as usize];
        harness.mem.write_at(header_gpa, &header_bytes).unwrap();
        let data_bytes = vec![0xABu8; 64];
        harness.mem.write_at(data_gpa, &data_bytes).unwrap();

        post_tx_packet(
            &harness.mem,
            desc_index,
            header_gpa,
            NET_HEADER_SIZE,
            &[(data_gpa, 64)],
        );
        make_available(
            &harness.mem,
            TX_AVAIL_ADDR,
            desc_index,
            &mut harness.tx_avail_idx,
        );
    }
    // Signal once for all 3
    harness.tx_event.signal();

    // First 2 packets should complete, third should not
    let (id0, _) = harness.wait_for_used().await;
    assert_eq!(id0, 0);

    let (id1, _) = harness.wait_for_used().await;
    assert_eq!(id1, 2);

    // Third should not be in used ring yet — we know the device has
    // processed through the second packet (we got it from the used ring),
    // so any further processing would be synchronous. Just check the ring.
    assert!(
        read_used(&harness.mem, &mut harness.tx_used_idx).is_none(),
        "third packet should not be completed yet"
    );

    // Reconfigure to accept all, then wake
    {
        let mut behavior = handle.tx_avail_behavior.lock();
        behavior.consume_all = true;
        behavior.consume_count = None;
    }
    // Wake the backend so it retries
    handle.complete_tx(vec![]);

    let (id2, _) = harness.wait_for_used().await;
    assert_eq!(id2, 4);
}

// --- RX Tests ---

/// Post 1 RX buffer, inject 1 packet, verify the used ring entry and that
/// the packet data was written to guest memory.
#[async_test]
async fn rx_single_packet(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    let mut handle = harness.enable_and_get_handle().await;

    // Post a 1500-byte RX buffer (enough for header + data)
    let buffer_size: u32 = 1500;
    let desc_index: u16 = 0;
    let gpa = harness.post_rx_buffer_and_signal(desc_index, buffer_size);

    // Wait for the device to pick up the buffer and call rx_avail
    handle.wait_for_rx_pending().await;

    // Inject a packet
    let payload = b"Hello, virtio-net RX!";
    handle.inject_rx_packet(payload);

    // Wait for the buffer to appear in the RX used ring
    let (used_id, used_len) = harness.wait_for_rx_used().await;
    assert_eq!(used_id, desc_index);
    // used_len should be header_size + payload length
    assert_eq!(used_len, NET_HEADER_SIZE + payload.len() as u32);

    // Verify the payload was written after the virtio-net header
    let mut readback = vec![0u8; payload.len()];
    harness
        .mem
        .read_at(gpa + NET_HEADER_SIZE as u64, &mut readback)
        .unwrap();
    assert_eq!(&readback, payload, "RX payload mismatch in guest memory");
}

/// Post 3 RX buffers, inject 3 packets sequentially, verify each completes in
/// order and the correct data is written.
#[async_test]
async fn rx_multiple_sequential_packets(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    let mut handle = harness.enable_and_get_handle().await;

    let buffer_size: u32 = 512;
    let mut gpas = Vec::new();

    // Post 3 RX buffers
    for i in 0u16..3 {
        let gpa = harness.post_rx_buffer_and_signal(i, buffer_size);
        gpas.push(gpa);
    }

    // Wait for the device to pick up all buffers
    for _ in 0..3 {
        handle.wait_for_rx_pending().await;
    }

    // Inject 3 packets with different data
    let payloads: [&[u8]; 3] = [b"packet-zero", b"packet-one!!", b"pkt-two"];
    for payload in &payloads {
        handle.inject_rx_packet(payload);
    }

    // Verify each used ring entry and guest memory content
    for (i, payload) in payloads.iter().enumerate() {
        let (used_id, used_len) = harness.wait_for_rx_used().await;
        assert_eq!(used_id, i as u16, "packet {} used id mismatch", i);
        assert_eq!(
            used_len,
            NET_HEADER_SIZE + payload.len() as u32,
            "packet {} used len mismatch",
            i
        );

        let mut readback = vec![0u8; payload.len()];
        harness
            .mem
            .read_at(gpas[i] + NET_HEADER_SIZE as u64, &mut readback)
            .unwrap();
        assert_eq!(&readback, payload, "packet {} data mismatch", i);
    }
}

/// Post an RX buffer, inject a large packet that fills most of the buffer.
/// Verify header and data integrity.
#[async_test]
async fn rx_large_payload(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    let mut handle = harness.enable_and_get_handle().await;

    let data_len: u32 = 1400;
    let buffer_size: u32 = NET_HEADER_SIZE + data_len;
    let desc_index: u16 = 0;
    let gpa = harness.post_rx_buffer_and_signal(desc_index, buffer_size);

    // Wait for the device to pick up the buffer
    handle.wait_for_rx_pending().await;

    // Create a payload with a recognizable byte pattern
    let payload: Vec<u8> = (0..data_len).map(|i| (i % 256) as u8).collect();
    handle.inject_rx_packet(&payload);

    let (used_id, used_len) = harness.wait_for_rx_used().await;
    assert_eq!(used_id, desc_index);
    assert_eq!(used_len, NET_HEADER_SIZE + data_len);

    let mut readback = vec![0u8; data_len as usize];
    harness
        .mem
        .read_at(gpa + NET_HEADER_SIZE as u64, &mut readback)
        .unwrap();
    assert_eq!(readback, payload, "large payload data mismatch");
}

/// Disable and re-enable the device, then send a packet.
/// This tests that poll_disable properly cleans up coordinator state so that
/// a subsequent enable() can re-insert it without panicking.
#[async_test]
async fn disable_and_reenable(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);

    // First enable cycle: enable, send a packet, verify it works.
    let handle = harness.enable_and_get_handle().await;
    harness.post_tx_and_signal(0, 64);
    let (used_id, _) = harness.wait_for_used().await;
    assert_eq!(used_id, 0);
    drop(handle);

    // Disable the device.
    harness.disable().await;

    // Re-enable: this would panic ("attempt to insert already-present state")
    // if poll_disable didn't remove the coordinator state.
    harness.reset_rings();
    let handle = harness.enable_and_get_handle().await;

    // Verify the device works after re-enable by sending another packet.
    harness.post_tx_and_signal(0, 64);
    let (used_id, _) = harness.wait_for_used().await;
    assert_eq!(used_id, 0);
    drop(handle);
}
