// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the virtio-console device.
//!
//! These tests construct a full `VirtioConsoleDevice` with guest memory, a mock
//! serial IO backend, and real virtio queues — then drive requests through the
//! descriptor rings just as a guest driver would.

use crate::VirtioConsoleDevice;
use futures::AsyncRead;
use futures::AsyncWrite;
use guestmem::GuestMemory;
use inspect::InspectMut;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::wait::PolledWait;
use pal_event::Event;
use parking_lot::Mutex;
use serial_core::SerialIo;
use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;
use test_with_tracing::test;
use virtio::QueueResources;
use virtio::VirtioDevice;
use virtio::queue::QueueParams;
use virtio::spec::VirtioDeviceFeatures;
use virtio::spec::queue::DescriptorFlags;
use vmcore::interrupt::Interrupt;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;

// --- Constants ---

const QUEUE_SIZE: u16 = 16;

// Memory layout for queue 0 (receiveq: host→guest)
const RX_DESC_ADDR: u64 = 0x0000;
const RX_AVAIL_ADDR: u64 = 0x1000;
const RX_USED_ADDR: u64 = 0x2000;

// Memory layout for queue 1 (transmitq: guest→host)
const TX_DESC_ADDR: u64 = 0x10000;
const TX_AVAIL_ADDR: u64 = 0x11000;
const TX_USED_ADDR: u64 = 0x12000;

// Data area for payloads
const DATA_BASE: u64 = 0x20000;
const TOTAL_MEM_SIZE: usize = 0x30000;

// --- MockSerialIo ---

/// Shared state for the mock serial backend.
struct MockShared {
    connected: bool,
    /// Data available for the device to read (backend→guest direction).
    rx_buf: VecDeque<u8>,
    /// Data written by the device (guest→backend direction).
    tx_buf: Vec<u8>,
    /// Waker registered by poll_read when rx_buf is empty.
    rx_waker: Option<Waker>,
    /// Waker registered by poll_connect when disconnected.
    connect_waker: Option<Waker>,
    /// Waker registered by poll_disconnect when connected.
    disconnect_waker: Option<Waker>,
    /// If set, the next poll_write accepts at most this many bytes, then
    /// auto-disconnects. Used to test partial-write-then-disconnect scenarios.
    write_limit_then_disconnect: Option<usize>,
    /// If set, each poll_write accepts at most this many bytes (persistent).
    max_write_size: Option<usize>,
}

/// A mock `SerialIo` implementation backed by shared state.
///
/// The device side uses this via the `SerialIo` trait; the test side uses
/// `MockSerialHandle` to inject data, read written data, and control
/// connect/disconnect.
struct MockSerialIo {
    shared: Arc<Mutex<MockShared>>,
}

impl InspectMut for MockSerialIo {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.ignore();
    }
}

impl SerialIo for MockSerialIo {
    fn is_connected(&self) -> bool {
        self.shared.lock().connected
    }

    fn poll_connect(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut shared = self.shared.lock();
        if shared.connected {
            Poll::Ready(Ok(()))
        } else {
            shared.connect_waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    fn poll_disconnect(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut shared = self.shared.lock();
        if !shared.connected {
            Poll::Ready(Ok(()))
        } else {
            shared.disconnect_waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl AsyncRead for MockSerialIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut shared = self.shared.lock();
        if !shared.connected {
            return Poll::Ready(Ok(0)); // EOF = disconnected
        }
        if shared.rx_buf.is_empty() {
            shared.rx_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let n = buf.len().min(shared.rx_buf.len());
        for byte in buf.iter_mut().take(n) {
            *byte = shared.rx_buf.pop_front().unwrap();
        }
        Poll::Ready(Ok(n))
    }
}

impl AsyncWrite for MockSerialIo {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut shared = self.shared.lock();
        if !shared.connected {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        if let Some(limit) = shared.write_limit_then_disconnect.take() {
            let n = buf.len().min(limit);
            shared.tx_buf.extend_from_slice(&buf[..n]);
            // Auto-disconnect after this partial write.
            shared.connected = false;
            if let Some(w) = shared.rx_waker.take() {
                w.wake();
            }
            if let Some(w) = shared.disconnect_waker.take() {
                w.wake();
            }
            return Poll::Ready(Ok(n));
        }
        let max = shared.max_write_size.unwrap_or(usize::MAX);
        let n = buf.len().min(max);
        shared.tx_buf.extend_from_slice(&buf[..n]);
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Test-side handle for controlling the mock serial backend.
struct MockSerialHandle {
    shared: Arc<Mutex<MockShared>>,
}

impl MockSerialHandle {
    /// Inject data that the device will read (backend→guest / receiveq direction).
    fn inject_rx_data(&self, data: &[u8]) {
        let mut shared = self.shared.lock();
        shared.rx_buf.extend(data);
        if let Some(w) = shared.rx_waker.take() {
            w.wake();
        }
    }

    /// Take all data written by the device (guest→backend / transmitq direction).
    fn take_tx_data(&self) -> Vec<u8> {
        let mut shared = self.shared.lock();
        std::mem::take(&mut shared.tx_buf)
    }

    /// Simulate backend disconnect.
    fn disconnect(&self) {
        let mut shared = self.shared.lock();
        shared.connected = false;
        // Wake the reader so it sees EOF.
        if let Some(w) = shared.rx_waker.take() {
            w.wake();
        }
        if let Some(w) = shared.disconnect_waker.take() {
            w.wake();
        }
    }

    /// Simulate backend reconnect.
    fn reconnect(&self) {
        let mut shared = self.shared.lock();
        shared.connected = true;
        if let Some(w) = shared.connect_waker.take() {
            w.wake();
        }
    }

    /// Arrange for the next poll_write to accept at most `limit` bytes and
    /// then auto-disconnect.
    fn set_write_limit_then_disconnect(&self, limit: usize) {
        self.shared.lock().write_limit_then_disconnect = Some(limit);
    }

    /// Set a persistent maximum write size for each poll_write call.
    fn set_max_write_size(&self, max: usize) {
        self.shared.lock().max_write_size = Some(max);
    }
}

fn new_mock_serial() -> (MockSerialIo, MockSerialHandle) {
    let shared = Arc::new(Mutex::new(MockShared {
        connected: true,
        rx_buf: VecDeque::new(),
        tx_buf: Vec::new(),
        rx_waker: None,
        connect_waker: None,
        disconnect_waker: None,
        write_limit_then_disconnect: None,
        max_write_size: None,
    }));
    (
        MockSerialIo {
            shared: shared.clone(),
        },
        MockSerialHandle { shared },
    )
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

/// Make a descriptor index available in the avail ring and bump the index.
fn make_available(mem: &GuestMemory, avail_addr: u64, desc_index: u16, avail_idx: &mut u16) {
    let ring_offset = avail_addr + 4 + 2 * (*avail_idx % QUEUE_SIZE) as u64;
    mem.write_at(ring_offset, &desc_index.to_le_bytes())
        .unwrap();
    *avail_idx = avail_idx.wrapping_add(1);
    mem.write_at(avail_addr + 2, &avail_idx.to_le_bytes())
        .unwrap();
}

/// Read the used ring index.
fn read_used_idx(mem: &GuestMemory, used_addr: u64) -> u16 {
    let mut buf = [0u8; 2];
    mem.read_at(used_addr + 2, &mut buf).unwrap();
    u16::from_le_bytes(buf)
}

/// Read a used ring entry (id, len).
fn read_used_entry(mem: &GuestMemory, used_addr: u64, index: u16) -> (u32, u32) {
    let entry_offset = used_addr + 4 + 8 * (index % QUEUE_SIZE) as u64;
    let mut id_buf = [0u8; 4];
    let mut len_buf = [0u8; 4];
    mem.read_at(entry_offset, &mut id_buf).unwrap();
    mem.read_at(entry_offset + 4, &mut len_buf).unwrap();
    (u32::from_le_bytes(id_buf), u32::from_le_bytes(len_buf))
}

/// Read the next TX used ring entry, returning (desc_id, bytes_written) or None.
fn read_tx_used(mem: &GuestMemory, used_idx: &mut u16) -> Option<(u16, u32)> {
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
    device: VirtioConsoleDevice,
    mem: GuestMemory,
    driver: DefaultDriver,
    handle: MockSerialHandle,
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

        init_avail_ring(&mem, RX_AVAIL_ADDR);
        init_used_ring(&mem, RX_USED_ADDR);
        init_avail_ring(&mem, TX_AVAIL_ADDR);
        init_used_ring(&mem, TX_USED_ADDR);

        let (io, handle) = new_mock_serial();

        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
        let device = VirtioConsoleDevice::new(&driver_source, Box::new(io));

        let rx_event = Event::new();
        let rx_interrupt_event = Event::new();
        let tx_event = Event::new();
        let tx_interrupt_event = Event::new();

        Self {
            device,
            mem,
            driver: driver.clone(),
            handle,
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

    /// Enable the device with both queues.
    async fn enable(&mut self) {
        let features = VirtioDeviceFeatures::new();

        // Queue 0: receiveq (host→guest)
        self.device
            .start_queue(
                0,
                QueueResources {
                    params: QueueParams {
                        size: QUEUE_SIZE,
                        enable: true,
                        desc_addr: RX_DESC_ADDR,
                        avail_addr: RX_AVAIL_ADDR,
                        used_addr: RX_USED_ADDR,
                    },
                    notify: Interrupt::from_event(self.rx_interrupt_event.clone()),
                    event: self.rx_event.clone(),
                    guest_memory: self.mem.clone(),
                },
                &features,
                None,
            )
            .await
            .unwrap();

        // Queue 1: transmitq (guest→host)
        self.device
            .start_queue(
                1,
                QueueResources {
                    params: QueueParams {
                        size: QUEUE_SIZE,
                        enable: true,
                        desc_addr: TX_DESC_ADDR,
                        avail_addr: TX_AVAIL_ADDR,
                        used_addr: TX_USED_ADDR,
                    },
                    notify: Interrupt::from_event(self.tx_interrupt_event.clone()),
                    event: self.tx_event.clone(),
                    guest_memory: self.mem.clone(),
                },
                &features,
                None,
            )
            .await
            .unwrap();
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

    /// Post a TX descriptor (guest→host write) with the given data.
    fn post_tx_and_signal(&mut self, desc_index: u16, data: &[u8]) {
        let gpa = self.alloc_data(data.len() as u32);
        self.mem.write_at(gpa, data).unwrap();

        // Single readable descriptor
        let flags = DescriptorFlags::new();
        write_descriptor(
            &self.mem,
            TX_DESC_ADDR,
            desc_index,
            gpa,
            data.len() as u32,
            flags,
            0,
        );

        make_available(&self.mem, TX_AVAIL_ADDR, desc_index, &mut self.tx_avail_idx);
        self.tx_event.signal();
    }

    /// Post a single writeable RX buffer descriptor, make it available, and
    /// signal the receiveq so the device picks it up.
    fn post_rx_buffer_and_signal(&mut self, desc_index: u16, buffer_size: u32) -> u64 {
        let gpa = self.alloc_data(buffer_size);
        // Zero the buffer so we can detect writes
        let zeroes = vec![0u8; buffer_size as usize];
        self.mem.write_at(gpa, &zeroes).unwrap();

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

    /// Wait for the next TX used ring entry with a timeout.
    async fn wait_for_tx_used(&mut self) -> (u16, u32) {
        let mut wait = PolledWait::new(&self.driver, self.tx_interrupt_event.clone()).unwrap();
        mesh::CancelContext::new()
            .with_timeout(Duration::from_secs(5))
            .until_cancelled(async {
                loop {
                    if let Some(entry) = read_tx_used(&self.mem, &mut self.tx_used_idx) {
                        return entry;
                    }
                    wait.wait().await.unwrap();
                }
            })
            .await
            .expect("timed out waiting for TX used ring entry")
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

    /// Disable the device.
    async fn disable(&mut self) {
        self.device.stop_queue(0).await;
        self.device.stop_queue(1).await;
        self.device.reset().await;
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

/// Guest writes data via the transmitq; verify the mock backend receives it.
#[async_test]
async fn guest_tx_basic(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;

    let payload = b"Hello from guest!";
    harness.post_tx_and_signal(0, payload);

    let (used_id, _used_len) = harness.wait_for_tx_used().await;
    assert_eq!(used_id, 0);

    // The worker should have forwarded the data to the backend.
    let written = harness.handle.take_tx_data();
    assert_eq!(written, payload);
}

/// Backend injects data; guest receives it via the receiveq.
#[async_test]
async fn guest_rx_basic(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;

    let buffer_size: u32 = 256;
    let gpa = harness.post_rx_buffer_and_signal(0, buffer_size);

    // Inject data from the backend side.
    let payload = b"Hello from host!";
    harness.handle.inject_rx_data(payload);

    let (used_id, used_len) = harness.wait_for_rx_used().await;
    assert_eq!(used_id, 0);
    assert_eq!(used_len, payload.len() as u32);

    // Verify the payload landed in guest memory.
    let mut readback = vec![0u8; payload.len()];
    harness.mem.read_at(gpa, &mut readback).unwrap();
    assert_eq!(&readback, payload);
}

/// Guest sends three messages sequentially; verify all arrive.
#[async_test]
async fn guest_tx_multiple(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;

    let messages: [&[u8]; 3] = [b"one", b"two", b"three"];

    for (i, msg) in messages.iter().enumerate() {
        harness.post_tx_and_signal(i as u16, msg);
        let (used_id, _) = harness.wait_for_tx_used().await;
        assert_eq!(used_id, i as u16, "message {} used id mismatch", i);
    }

    // All messages should have been forwarded, concatenated in the tx_buf.
    let written = harness.handle.take_tx_data();
    assert_eq!(written, b"onetwothree");
}

/// Backend sends multiple chunks; guest receives each in its own RX buffer.
#[async_test]
async fn guest_rx_multiple(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;

    let payloads: [&[u8]; 3] = [b"alpha", b"beta!", b"gamma"];

    for (i, payload) in payloads.iter().enumerate() {
        // Post an RX buffer, then inject data.
        let gpa = harness.post_rx_buffer_and_signal(i as u16, 64);
        harness.handle.inject_rx_data(payload);

        let (used_id, used_len) = harness.wait_for_rx_used().await;
        assert_eq!(used_id, i as u16, "payload {} used id mismatch", i);
        assert_eq!(used_len, payload.len() as u32);

        let mut readback = vec![0u8; payload.len()];
        harness.mem.read_at(gpa, &mut readback).unwrap();
        assert_eq!(&readback, payload, "payload {} data mismatch", i);
    }
}

/// A large payload (close to BUF_SIZE = 4096) is transferred correctly.
#[async_test]
async fn guest_rx_large_payload(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;

    let data_len: u32 = 4000;
    let gpa = harness.post_rx_buffer_and_signal(0, data_len);

    let payload: Vec<u8> = (0..data_len).map(|i| (i % 256) as u8).collect();
    harness.handle.inject_rx_data(&payload);

    let (used_id, used_len) = harness.wait_for_rx_used().await;
    assert_eq!(used_id, 0);
    assert_eq!(used_len, data_len);

    let mut readback = vec![0u8; data_len as usize];
    harness.mem.read_at(gpa, &mut readback).unwrap();
    assert_eq!(readback, payload);
}

/// When the backend is disconnected, guest TX data is drained (consumed
/// without forwarding). After reconnect, TX works again.
#[async_test]
async fn disconnect_drains_tx_then_reconnect(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;

    // Verify TX works while connected.
    harness.post_tx_and_signal(0, b"before-disconnect");
    let (used_id, _) = harness.wait_for_tx_used().await;
    assert_eq!(used_id, 0);
    let written = harness.handle.take_tx_data();
    assert_eq!(written, b"before-disconnect");

    // Disconnect the backend.
    harness.handle.disconnect();

    // TX data should be drained (consumed by the device without forwarding).
    harness.post_tx_and_signal(1, b"during-disconnect");
    let (used_id, _) = harness.wait_for_tx_used().await;
    assert_eq!(used_id, 1);

    // Nothing should have been forwarded.
    let written = harness.handle.take_tx_data();
    assert!(written.is_empty(), "data should be drained, not forwarded");

    // Reconnect.
    harness.handle.reconnect();

    // TX should work again.
    harness.post_tx_and_signal(2, b"after-reconnect");
    let (used_id, _) = harness.wait_for_tx_used().await;
    assert_eq!(used_id, 2);
    let written = harness.handle.take_tx_data();
    assert_eq!(written, b"after-reconnect");
}

/// Disable and re-enable the device. Verify it still works after re-enable.
#[async_test]
async fn disable_and_reenable(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;

    // Send a TX message to verify the device is working.
    harness.post_tx_and_signal(0, b"first-cycle");
    let (used_id, _) = harness.wait_for_tx_used().await;
    assert_eq!(used_id, 0);
    let written = harness.handle.take_tx_data();
    assert_eq!(written, b"first-cycle");

    // Disable the device.
    harness.disable().await;

    // Re-enable.
    harness.reset_rings();
    harness.enable().await;

    // Verify the device works after re-enable.
    harness.post_tx_and_signal(0, b"second-cycle");
    let (used_id, _) = harness.wait_for_tx_used().await;
    assert_eq!(used_id, 0);
    let written = harness.handle.take_tx_data();
    assert_eq!(written, b"second-cycle");
}

/// Traits report correct device ID, queue count, and feature bits.
#[async_test]
async fn traits_are_correct(driver: DefaultDriver) {
    let (io, _handle) = new_mock_serial();
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let device = VirtioConsoleDevice::new(&driver_source, Box::new(io));
    let traits = device.traits();
    assert_eq!(traits.device_id, virtio::spec::VirtioDeviceType::CONSOLE);
    assert_eq!(traits.max_queues, 2); // receiveq + transmitq
}

/// TX used ring entries must report len=0 (device writes nothing back to
/// guest memory for OUT-only transmit descriptors).
#[async_test]
async fn guest_tx_used_len_is_zero(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;

    harness.post_tx_and_signal(0, b"test payload");
    let (used_id, used_len) = harness.wait_for_tx_used().await;
    assert_eq!(used_id, 0);
    assert_eq!(
        used_len, 0,
        "TX used len must be 0 for OUT-only descriptors"
    );
}

/// A TX payload larger than BUF_SIZE (4096) must be forwarded in full.
#[async_test]
async fn guest_tx_large_payload(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;

    let payload: Vec<u8> = (0..8000u16).map(|i| (i % 256) as u8).collect();
    harness.post_tx_and_signal(0, &payload);

    let (used_id, _) = harness.wait_for_tx_used().await;
    assert_eq!(used_id, 0);

    let written = harness.handle.take_tx_data();
    assert_eq!(written.len(), payload.len(), "all bytes must be forwarded");
    assert_eq!(written, payload);
}

/// A TX payload larger than BUF_SIZE with the backend doing small partial
/// writes. This exercises the read_at_offset chunking path.
#[async_test]
async fn guest_tx_large_payload_partial_writes(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;

    // Limit each poll_write to 100 bytes to force many iterations.
    harness.handle.set_max_write_size(100);

    let payload: Vec<u8> = (0..10000u16).map(|i| (i % 256) as u8).collect();
    harness.post_tx_and_signal(0, &payload);

    let (used_id, _) = harness.wait_for_tx_used().await;
    assert_eq!(used_id, 0);

    let written = harness.handle.take_tx_data();
    assert_eq!(written.len(), payload.len(), "all bytes must be forwarded");
    assert_eq!(written, payload);
}

/// After a partial write followed by disconnect, the drain loop consumes the
/// in-progress descriptor and resets partial_transmit. Verify that the next
/// descriptor after reconnect is forwarded correctly from the beginning.
#[async_test]
async fn tx_partial_write_disconnect_reconnect(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;

    // Arrange: the next write accepts 3 bytes then auto-disconnects.
    harness.handle.set_write_limit_then_disconnect(3);

    // Post a TX descriptor. The device will write 3 bytes ("abc"), then the
    // backend disconnects. The drain loop will consume the descriptor.
    harness.post_tx_and_signal(0, b"abcdef");
    let (used_id, _) = harness.wait_for_tx_used().await;
    assert_eq!(used_id, 0);

    // The mock received only the partial write.
    let written = harness.handle.take_tx_data();
    assert_eq!(written, b"abc");

    // Reconnect.
    harness.handle.reconnect();

    // Send a fresh descriptor. It must arrive in full — partial_transmit
    // must have been reset when the drain loop consumed descriptor 0.
    harness.post_tx_and_signal(1, b"xyz123");
    let (used_id, _) = harness.wait_for_tx_used().await;
    assert_eq!(used_id, 1);

    let written = harness.handle.take_tx_data();
    assert_eq!(
        written, b"xyz123",
        "partial_transmit must be reset by drain loop"
    );
}

/// A zero-length writeable RX descriptor must not cause a false disconnect.
/// The device should consume it with len=0 and continue processing.
#[async_test]
async fn rx_zero_length_buffer_no_disconnect(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;

    // Post a zero-length writeable RX buffer.
    harness.post_rx_buffer_and_signal(0, 0);

    // It should be completed immediately with len=0.
    let (used_id, used_len) = harness.wait_for_rx_used().await;
    assert_eq!(used_id, 0);
    assert_eq!(used_len, 0);

    // The device must still be connected — verify by doing a normal RX.
    let gpa = harness.post_rx_buffer_and_signal(1, 64);
    harness.handle.inject_rx_data(b"still connected");

    let (used_id, used_len) = harness.wait_for_rx_used().await;
    assert_eq!(used_id, 1);
    assert_eq!(used_len, b"still connected".len() as u32);

    let mut readback = vec![0u8; b"still connected".len()];
    harness.mem.read_at(gpa, &mut readback).unwrap();
    assert_eq!(&readback, b"still connected");
}

/// Config space read returns cols | (rows << 16) at offset 0.
#[async_test]
async fn config_read(driver: DefaultDriver) {
    let (io, _handle) = new_mock_serial();
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let mut device = VirtioConsoleDevice::new(&driver_source, Box::new(io));
    // Default config: cols=0, rows=0
    let val = device.read_registers_u32(0).await;
    assert_eq!(val, 0);
}

/// When only the transmitq (queue 1) is enabled, TX should still work.
#[async_test]
async fn tx_only_single_queue(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    let features = VirtioDeviceFeatures::new();

    // Only start queue 1 (transmitq).
    harness
        .device
        .start_queue(
            1,
            QueueResources {
                params: QueueParams {
                    size: QUEUE_SIZE,
                    enable: true,
                    desc_addr: TX_DESC_ADDR,
                    avail_addr: TX_AVAIL_ADDR,
                    used_addr: TX_USED_ADDR,
                },
                notify: Interrupt::from_event(harness.tx_interrupt_event.clone()),
                event: harness.tx_event.clone(),
                guest_memory: harness.mem.clone(),
            },
            &features,
            None,
        )
        .await
        .unwrap();

    harness.post_tx_and_signal(0, b"tx-only");
    let (used_id, _) = harness.wait_for_tx_used().await;
    assert_eq!(used_id, 0);

    let written = harness.handle.take_tx_data();
    assert_eq!(written, b"tx-only");
}

/// When only the receiveq (queue 0) is enabled, RX should still work.
#[async_test]
async fn rx_only_single_queue(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    let features = VirtioDeviceFeatures::new();

    // Only start queue 0 (receiveq).
    harness
        .device
        .start_queue(
            0,
            QueueResources {
                params: QueueParams {
                    size: QUEUE_SIZE,
                    enable: true,
                    desc_addr: RX_DESC_ADDR,
                    avail_addr: RX_AVAIL_ADDR,
                    used_addr: RX_USED_ADDR,
                },
                notify: Interrupt::from_event(harness.rx_interrupt_event.clone()),
                event: harness.rx_event.clone(),
                guest_memory: harness.mem.clone(),
            },
            &features,
            None,
        )
        .await
        .unwrap();

    let gpa = harness.post_rx_buffer_and_signal(0, 64);
    harness.handle.inject_rx_data(b"rx-only");

    let (used_id, used_len) = harness.wait_for_rx_used().await;
    assert_eq!(used_id, 0);
    assert_eq!(used_len, b"rx-only".len() as u32);

    let mut readback = vec![0u8; b"rx-only".len()];
    harness.mem.read_at(gpa, &mut readback).unwrap();
    assert_eq!(&readback, b"rx-only");
}
