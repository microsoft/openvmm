// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the virtio-vsock device.
//!
//! These tests construct a full `VirtioVsockDevice` with guest memory, real
//! virtio queues, and Unix domain socket listeners — then drive the vsock
//! handshake and data transfer through the descriptor rings, just as a guest
//! driver would.

use crate::VirtioVsockDevice;
use crate::spec::*;
use core::mem::offset_of;
use guestmem::GuestMemory;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::socket::PolledSocket;
use pal_async::wait::PolledWait;
use pal_event::Event;
use std::io::Read;
use std::io::Write;
use std::time::Duration;
use test_with_tracing::test;
use unix_socket::UnixListener;
use virtio::QueueResources;
use virtio::VirtioDevice;
use virtio::queue::QueueParams;
use virtio::spec::VirtioDeviceFeatures;
use virtio::spec::queue::AVAIL_ELEMENT_SIZE;
use virtio::spec::queue::AVAIL_OFFSET_FLAGS;
use virtio::spec::queue::AVAIL_OFFSET_IDX;
use virtio::spec::queue::AVAIL_OFFSET_RING;
use virtio::spec::queue::DescriptorFlags;
use virtio::spec::queue::SplitDescriptor;
use virtio::spec::queue::USED_ELEMENT_SIZE;
use virtio::spec::queue::USED_OFFSET_FLAGS;
use virtio::spec::queue::USED_OFFSET_IDX;
use virtio::spec::queue::USED_OFFSET_RING;
use virtio::spec::queue::UsedElement;
use vmcore::interrupt::Interrupt;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

// --- Constants ---

const QUEUE_SIZE: u16 = 256;
const GUEST_CID: u64 = 3;

// Memory layout for three virtqueues (rx=0, tx=1, event=2).
// Each queue has a descriptor table, avail ring, and used ring.
const RX_DESC_ADDR: u64 = 0x0000;
const RX_AVAIL_ADDR: u64 = 0x1000;
const RX_USED_ADDR: u64 = 0x2000;

const TX_DESC_ADDR: u64 = 0x3000;
const TX_AVAIL_ADDR: u64 = 0x4000;
const TX_USED_ADDR: u64 = 0x5000;

const EVENT_DESC_ADDR: u64 = 0x6000;
const EVENT_AVAIL_ADDR: u64 = 0x7000;
const EVENT_USED_ADDR: u64 = 0x8000;

// Data area for packet headers and payloads
const DATA_BASE: u64 = 0x10000;
const TOTAL_MEM_SIZE: usize = 0x100000;

// The vsock header is 44 bytes.
const HDR_SIZE: u32 = VSOCK_HEADER_SIZE as u32;

// Default buffer allocation advertised by the guest.
const GUEST_BUF_ALLOC: u32 = 65536;

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
    let base = desc_table_base + size_of::<SplitDescriptor>() as u64 * index as u64;
    mem.write_at(
        base + offset_of!(SplitDescriptor, address) as u64,
        &addr.to_le_bytes(),
    )
    .unwrap();
    mem.write_at(
        base + offset_of!(SplitDescriptor, length) as u64,
        &len.to_le_bytes(),
    )
    .unwrap();
    mem.write_at(
        base + offset_of!(SplitDescriptor, flags_raw) as u64,
        &u16::from(flags).to_le_bytes(),
    )
    .unwrap();
    mem.write_at(
        base + offset_of!(SplitDescriptor, next) as u64,
        &next.to_le_bytes(),
    )
    .unwrap();
}

/// Initialize avail ring (flags=0, idx=0).
fn init_avail_ring(mem: &GuestMemory, avail_addr: u64) {
    mem.write_at(avail_addr + AVAIL_OFFSET_FLAGS, &0u16.to_le_bytes())
        .unwrap();
    mem.write_at(avail_addr + AVAIL_OFFSET_IDX, &0u16.to_le_bytes())
        .unwrap();
}

/// Initialize used ring (flags=0, idx=0).
fn init_used_ring(mem: &GuestMemory, used_addr: u64) {
    mem.write_at(used_addr + USED_OFFSET_FLAGS, &0u16.to_le_bytes())
        .unwrap();
    mem.write_at(used_addr + USED_OFFSET_IDX, &0u16.to_le_bytes())
        .unwrap();
}

/// Make a descriptor index available in the avail ring and bump the index.
fn make_available(
    mem: &GuestMemory,
    avail_addr: u64,
    desc_index: u16,
    avail_idx: &mut u16,
    queue_size: u16,
) {
    let ring_offset =
        avail_addr + AVAIL_OFFSET_RING + AVAIL_ELEMENT_SIZE * (*avail_idx % queue_size) as u64;
    mem.write_at(ring_offset, &desc_index.to_le_bytes())
        .unwrap();
    *avail_idx = avail_idx.wrapping_add(1);
    mem.write_at(avail_addr + AVAIL_OFFSET_IDX, &avail_idx.to_le_bytes())
        .unwrap();
}

/// Read the used ring index.
fn read_used_idx(mem: &GuestMemory, used_addr: u64) -> u16 {
    let mut buf = [0u8; 2];
    mem.read_at(used_addr + USED_OFFSET_IDX, &mut buf).unwrap();
    u16::from_le_bytes(buf)
}

/// Read a used ring entry (id, len).
fn read_used_entry(mem: &GuestMemory, used_addr: u64, index: u16) -> (u32, u32) {
    let entry_offset =
        used_addr + USED_OFFSET_RING + USED_ELEMENT_SIZE * (index % QUEUE_SIZE) as u64;
    let mut id_buf = [0u8; 4];
    let mut len_buf = [0u8; 4];
    mem.read_at(
        entry_offset + offset_of!(UsedElement, id) as u64,
        &mut id_buf,
    )
    .unwrap();
    mem.read_at(
        entry_offset + offset_of!(UsedElement, len) as u64,
        &mut len_buf,
    )
    .unwrap();
    (u32::from_le_bytes(id_buf), u32::from_le_bytes(len_buf))
}

/// Read the next used ring entry, returning (desc_id, bytes_written) or None.
fn read_used(mem: &GuestMemory, used_addr: u64, used_idx: &mut u16) -> Option<(u16, u32)> {
    let current_used_idx = read_used_idx(mem, used_addr);
    if current_used_idx == *used_idx {
        return None;
    }
    let (id, len) = read_used_entry(mem, used_addr, *used_idx);
    *used_idx = used_idx.wrapping_add(1);
    Some((id as u16, len))
}

// --- Test Harness ---

struct TestHarness {
    device: VirtioVsockDevice,
    mem: GuestMemory,
    driver: DefaultDriver,
    // Per-queue kick events (guest -> device notifications)
    rx_queue_event: Event,
    tx_queue_event: Event,
    event_queue_event: Event,
    // Per-queue interrupt events (device -> guest notifications)
    rx_interrupt_event: Event,
    tx_interrupt_event: Event,
    // Track avail/used indices per queue
    rx_avail_idx: u16,
    rx_used_idx: u16,
    tx_avail_idx: u16,
    tx_used_idx: u16,
    // Descriptor index allocators per queue
    next_rx_desc: u16,
    next_tx_desc: u16,
    next_data_offset: u64,
    // Temporary directory for socket files
    _tmp_dir: tempfile::TempDir,
}

impl TestHarness {
    /// Create a harness with a Unix listener at the expected socket path for the
    /// given port, plus a separate listener for host-initiated connections.
    fn new(driver: &DefaultDriver, tmp_dir: tempfile::TempDir) -> Self {
        let mem = GuestMemory::allocate(TOTAL_MEM_SIZE);

        // Initialize all three queue ring structures.
        init_avail_ring(&mem, RX_AVAIL_ADDR);
        init_used_ring(&mem, RX_USED_ADDR);
        init_avail_ring(&mem, TX_AVAIL_ADDR);
        init_used_ring(&mem, TX_USED_ADDR);
        init_avail_ring(&mem, EVENT_AVAIL_ADDR);
        init_used_ring(&mem, EVENT_USED_ADDR);

        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));

        // Create the host-initiated connection listener (not used by most tests,
        // but required by the constructor).
        let host_listener_path = tmp_dir.path().join("host_listener.sock");
        let host_listener = UnixListener::bind(&host_listener_path).expect("bind host listener");

        let base_path = tmp_dir.path().join("vsock");
        let device = VirtioVsockDevice::new(&driver_source, GUEST_CID, base_path, host_listener)
            .expect("create vsock device");

        let rx_queue_event = Event::new();
        let tx_queue_event = Event::new();
        let event_queue_event = Event::new();
        let rx_interrupt_event = Event::new();
        let tx_interrupt_event = Event::new();

        Self {
            device,
            mem,
            driver: driver.clone(),
            rx_queue_event,
            tx_queue_event,
            event_queue_event,
            rx_interrupt_event,
            tx_interrupt_event,
            rx_avail_idx: 0,
            rx_used_idx: 0,
            tx_avail_idx: 0,
            tx_used_idx: 0,
            next_rx_desc: 0,
            next_tx_desc: 0,
            next_data_offset: DATA_BASE,
            _tmp_dir: tmp_dir,
        }
    }

    /// Enable the device by starting all three queues.
    async fn enable(&mut self) {
        let features = VirtioDeviceFeatures::new();

        // Start rx queue (index 0)
        let rx_interrupt = Interrupt::from_event(self.rx_interrupt_event.clone());
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
                    notify: rx_interrupt,
                    event: self.rx_queue_event.clone(),
                    guest_memory: self.mem.clone(),
                },
                &features,
                None,
            )
            .await
            .unwrap();

        // Start tx queue (index 1)
        let tx_interrupt = Interrupt::from_event(self.tx_interrupt_event.clone());
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
                    notify: tx_interrupt,
                    event: self.tx_queue_event.clone(),
                    guest_memory: self.mem.clone(),
                },
                &features,
                None,
            )
            .await
            .unwrap();

        // Start event queue (index 2) — uses tx interrupt since we don't
        // actively monitor events in these tests.
        let event_interrupt = Interrupt::from_event(Event::new());
        self.device
            .start_queue(
                2,
                QueueResources {
                    params: QueueParams {
                        size: QUEUE_SIZE,
                        enable: true,
                        desc_addr: EVENT_DESC_ADDR,
                        avail_addr: EVENT_AVAIL_ADDR,
                        used_addr: EVENT_USED_ADDR,
                    },
                    notify: event_interrupt,
                    event: self.event_queue_event.clone(),
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

    /// Build a vsock header for a guest-originated packet.
    fn guest_header(
        &self,
        src_port: u32,
        dst_port: u32,
        op: Operation,
        len: u32,
        flags: u32,
    ) -> VsockHeader {
        VsockHeader {
            src_cid: GUEST_CID,
            dst_cid: VSOCK_CID_HOST,
            src_port,
            dst_port,
            len,
            socket_type: SocketType::STREAM.0,
            op: op.0,
            flags,
            buf_alloc: GUEST_BUF_ALLOC,
            fwd_cnt: 0,
        }
    }

    /// Post a vsock packet on the tx queue (guest -> host).
    ///
    /// The packet consists of a single descriptor containing the header
    /// followed by optional data payload. Returns the descriptor index used.
    fn post_tx_packet(&mut self, header: &VsockHeader, data: &[u8]) -> u16 {
        let desc_idx = self.next_tx_desc;
        self.next_tx_desc += 1;

        let total_len = HDR_SIZE + data.len() as u32;
        let gpa = self.alloc_data(total_len);

        // Write header
        self.mem.write_at(gpa, header.as_bytes()).unwrap();
        // Write data payload after the header
        if !data.is_empty() {
            self.mem.write_at(gpa + HDR_SIZE as u64, data).unwrap();
        }

        // Single readable descriptor
        let flags = DescriptorFlags::new();
        write_descriptor(&self.mem, TX_DESC_ADDR, desc_idx, gpa, total_len, flags, 0);

        make_available(
            &self.mem,
            TX_AVAIL_ADDR,
            desc_idx,
            &mut self.tx_avail_idx,
            QUEUE_SIZE,
        );
        self.tx_queue_event.signal();

        desc_idx
    }

    /// Post a buffer on the rx queue for the device to fill (host -> guest).
    ///
    /// The buffer is a single writable descriptor with space for header + data.
    /// Returns (descriptor index, GPA of the buffer).
    fn post_rx_buffer(&mut self, buf_size: u32) -> (u16, u64) {
        let desc_idx = self.next_rx_desc;
        self.next_rx_desc += 1;

        let gpa = self.alloc_data(buf_size);
        // Zero the buffer
        let zeroes = vec![0u8; buf_size as usize];
        self.mem.write_at(gpa, &zeroes).unwrap();

        // Single writable descriptor
        let flags = DescriptorFlags::new().with_write(true);
        write_descriptor(&self.mem, RX_DESC_ADDR, desc_idx, gpa, buf_size, flags, 0);

        make_available(
            &self.mem,
            RX_AVAIL_ADDR,
            desc_idx,
            &mut self.rx_avail_idx,
            QUEUE_SIZE,
        );
        self.rx_queue_event.signal();

        (desc_idx, gpa)
    }

    /// Wait for the tx queue to consume a descriptor (used ring entry).
    async fn wait_for_tx_used(&mut self) -> (u16, u32) {
        let mut wait = PolledWait::new(&self.driver, self.tx_interrupt_event.clone()).unwrap();
        mesh::CancelContext::new()
            .with_timeout(Duration::from_secs(5))
            .until_cancelled(async {
                loop {
                    if let Some(entry) = read_used(&self.mem, TX_USED_ADDR, &mut self.tx_used_idx) {
                        return entry;
                    }
                    wait.wait().await.unwrap();
                }
            })
            .await
            .expect("timed out waiting for tx used ring entry")
    }

    /// Wait for the rx queue to produce a response (used ring entry).
    async fn wait_for_rx_used(&mut self) -> (u16, u32) {
        let mut wait = PolledWait::new(&self.driver, self.rx_interrupt_event.clone()).unwrap();
        mesh::CancelContext::new()
            .with_timeout(Duration::from_secs(5))
            .until_cancelled(async {
                loop {
                    if let Some(entry) = read_used(&self.mem, RX_USED_ADDR, &mut self.rx_used_idx) {
                        return entry;
                    }
                    wait.wait().await.unwrap();
                }
            })
            .await
            .expect("timed out waiting for rx used ring entry")
    }

    /// Read a VsockHeader from a guest memory address.
    fn read_header(&self, gpa: u64) -> VsockHeader {
        let mut buf = [0u8; VSOCK_HEADER_SIZE];
        self.mem.read_at(gpa, &mut buf).unwrap();
        *VsockHeader::ref_from_bytes(&buf).unwrap()
    }

    /// Read data bytes following a VsockHeader at the given GPA.
    fn read_rx_data(&self, gpa: u64, len: u32) -> Vec<u8> {
        let mut buf = vec![0u8; len as usize];
        self.mem.read_at(gpa + HDR_SIZE as u64, &mut buf).unwrap();
        buf
    }

    /// Create a Unix listener at the path the relay will use for a given port.
    fn create_port_listener(&self, port: u32) -> PolledSocket<UnixListener> {
        let socket_path = self._tmp_dir.path().join(format!("vsock_{port}"));
        let listener = UnixListener::bind(&socket_path).expect("bind port listener");
        PolledSocket::new(&self.driver, listener).unwrap()
    }

    /// Perform a full guest-initiated connection handshake.
    ///
    /// 1. Post rx buffers for the RESPONSE
    /// 2. Send REQUEST on tx queue
    /// 3. Accept the connection on the host listener (async, non-blocking)
    /// 4. Wait for tx to be consumed and rx RESPONSE to arrive
    ///
    /// Returns the accepted host-side stream.
    async fn connect_guest_to_host(
        &mut self,
        listener: &mut PolledSocket<UnixListener>,
        guest_port: u32,
        host_port: u32,
    ) -> unix_socket::UnixStream {
        // Post an rx buffer for the response.
        let (_rx_desc, rx_gpa) = self.post_rx_buffer(HDR_SIZE + 1024);

        // Send the connection request.
        let header = self.guest_header(guest_port, host_port, Operation::REQUEST, 0, 0);
        self.post_tx_packet(&header, &[]);

        // Accept the connection on the host side using async polling so the
        // device worker can make progress on the same async executor.
        let (stream, _) = mesh::CancelContext::new()
            .with_timeout(Duration::from_secs(5))
            .until_cancelled(listener.accept())
            .await
            .expect("timed out waiting for host accept")
            .expect("accept failed");

        // Wait for tx consumed.
        let (_tx_id, _tx_len) = self.wait_for_tx_used().await;

        // Wait for the RESPONSE on the rx queue.
        let (_rx_id, rx_len) = self.wait_for_rx_used().await;
        assert!(rx_len >= HDR_SIZE);

        let resp_hdr = self.read_header(rx_gpa);
        let op = resp_hdr.op;
        let src_port = resp_hdr.src_port;
        let dst_port = resp_hdr.dst_port;
        let dst_cid = resp_hdr.dst_cid;
        assert_eq!(Operation(op), Operation::RESPONSE);
        assert_eq!(src_port, host_port);
        assert_eq!(dst_port, guest_port);
        assert_eq!(dst_cid, GUEST_CID);

        stream
    }
}

// --- Tests ---

/// Test that the device can be constructed and its config space returns the
/// correct guest CID.
#[async_test]
async fn config_returns_guest_cid(driver: DefaultDriver) {
    let tmp_dir = tempfile::tempdir().unwrap();
    let mut harness = TestHarness::new(&driver, tmp_dir);

    let lo = harness.device.read_registers_u32(0).await;
    let hi = harness.device.read_registers_u32(4).await;
    let cid = lo as u64 | ((hi as u64) << 32);
    assert_eq!(cid, GUEST_CID);
}

/// Guest-initiated connection: guest sends REQUEST, device connects to the
/// host Unix socket and sends RESPONSE back to the guest.
#[async_test]
async fn guest_connect_handshake(driver: DefaultDriver) {
    let tmp_dir = tempfile::tempdir().unwrap();
    let mut harness = TestHarness::new(&driver, tmp_dir);
    let mut listener = harness.create_port_listener(5000);
    harness.enable().await;

    let _stream = harness
        .connect_guest_to_host(&mut listener, 1024, 5000)
        .await;
}

/// Guest sends data to the host through an established connection.
#[async_test]
async fn guest_to_host_data(driver: DefaultDriver) {
    let tmp_dir = tempfile::tempdir().unwrap();
    let mut harness = TestHarness::new(&driver, tmp_dir);
    let mut listener = harness.create_port_listener(5001);
    harness.enable().await;

    let mut stream = harness
        .connect_guest_to_host(&mut listener, 1024, 5001)
        .await;

    // Send data from the guest.
    let payload = b"hello from guest";
    let header = harness.guest_header(1024, 5001, Operation::RW, payload.len() as u32, 0);
    harness.post_tx_packet(&header, payload);
    let (_tx_id, _tx_len) = harness.wait_for_tx_used().await;

    // Read the data on the host side.
    let mut buf = vec![0u8; payload.len()];
    stream.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, payload);
}

/// Host sends data to the guest through an established connection.
#[async_test]
async fn host_to_guest_data(driver: DefaultDriver) {
    let tmp_dir = tempfile::tempdir().unwrap();
    let mut harness = TestHarness::new(&driver, tmp_dir);
    let mut listener = harness.create_port_listener(5002);
    harness.enable().await;

    let mut stream = harness
        .connect_guest_to_host(&mut listener, 1024, 5002)
        .await;

    // Post rx buffers for the incoming data.
    let (_rx_desc, rx_gpa) = harness.post_rx_buffer(HDR_SIZE + 4096);

    // Write data from the host side.
    let payload = b"hello from host";
    stream.write_all(payload).unwrap();

    // Wait for the device to deliver the data to the guest rx queue.
    let (_rx_id, rx_len) = harness.wait_for_rx_used().await;
    assert!(rx_len >= HDR_SIZE + payload.len() as u32);

    let rx_hdr = harness.read_header(rx_gpa);
    let op = rx_hdr.op;
    let data_len = rx_hdr.len;
    assert_eq!(Operation(op), Operation::RW);
    assert_eq!(data_len, payload.len() as u32);

    let data = harness.read_rx_data(rx_gpa, data_len);
    assert_eq!(&data, payload);
}

/// Bidirectional data transfer: guest writes, host echoes, guest reads back.
#[async_test]
async fn bidirectional_echo(driver: DefaultDriver) {
    let tmp_dir = tempfile::tempdir().unwrap();
    let mut harness = TestHarness::new(&driver, tmp_dir);
    let mut listener = harness.create_port_listener(5003);
    harness.enable().await;

    let mut stream = harness
        .connect_guest_to_host(&mut listener, 1024, 5003)
        .await;

    // Post rx buffers upfront so the device can deliver credit updates and
    // data packets without blocking.
    let mut rx_gpas = Vec::new();
    for _ in 0..4 {
        let (_desc, gpa) = harness.post_rx_buffer(HDR_SIZE + 4096);
        rx_gpas.push(gpa);
    }

    // Guest sends data.
    let payload = b"echo me!";
    let header = harness.guest_header(1024, 5003, Operation::RW, payload.len() as u32, 0);
    harness.post_tx_packet(&header, payload);
    harness.wait_for_tx_used().await;

    // Host reads and echoes back.
    let mut buf = vec![0u8; payload.len()];
    stream.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, payload);
    stream.write_all(&buf).unwrap();

    // Read rx used entries until we find the RW data packet, skipping
    // intermediate control packets (e.g. CREDIT_UPDATE).
    let mut rx_buf_idx = 0;
    loop {
        let (_rx_id, rx_len) = harness.wait_for_rx_used().await;
        let gpa = rx_gpas[rx_buf_idx];
        rx_buf_idx += 1;

        let rx_hdr = harness.read_header(gpa);
        let op = rx_hdr.op;
        if Operation(op) == Operation::RW {
            let data_len = rx_hdr.len;
            assert!(rx_len >= HDR_SIZE + data_len);
            let data = harness.read_rx_data(gpa, data_len);
            assert_eq!(&data, payload);
            break;
        }
        // Otherwise it's a control packet (credit update, etc.) — skip.
    }
}

/// REQUEST to a port with no listener results in RST.
#[async_test]
async fn connect_to_nonexistent_port_gets_rst(driver: DefaultDriver) {
    let tmp_dir = tempfile::tempdir().unwrap();
    let mut harness = TestHarness::new(&driver, tmp_dir);
    // No listener for port 9999.
    harness.enable().await;

    // Post an rx buffer for the RST.
    let (_rx_desc, rx_gpa) = harness.post_rx_buffer(HDR_SIZE + 64);

    // Send the connection request.
    let header = harness.guest_header(1024, 9999, Operation::REQUEST, 0, 0);
    harness.post_tx_packet(&header, &[]);

    // Wait for tx consumed.
    harness.wait_for_tx_used().await;

    // Wait for the RST on the rx queue.
    let (_rx_id, _rx_len) = harness.wait_for_rx_used().await;

    let rst_hdr = harness.read_header(rx_gpa);
    let op = rst_hdr.op;
    let dst_port = rst_hdr.dst_port;
    assert_eq!(Operation(op), Operation::RST);
    assert_eq!(dst_port, 1024);
}

/// Multiple simultaneous connections to different ports.
#[async_test]
async fn multiple_simultaneous_connections(driver: DefaultDriver) {
    let tmp_dir = tempfile::tempdir().unwrap();
    let mut harness = TestHarness::new(&driver, tmp_dir);
    let mut listener_a = harness.create_port_listener(6000);
    let mut listener_b = harness.create_port_listener(6001);
    harness.enable().await;

    // Establish connection A (guest port 1000 -> host port 6000).
    let mut stream_a = harness
        .connect_guest_to_host(&mut listener_a, 1000, 6000)
        .await;

    // Establish connection B (guest port 1001 -> host port 6001).
    let mut stream_b = harness
        .connect_guest_to_host(&mut listener_b, 1001, 6001)
        .await;

    // Send data on connection A.
    let payload_a = b"connection A data";
    let header_a = harness.guest_header(1000, 6000, Operation::RW, payload_a.len() as u32, 0);
    harness.post_tx_packet(&header_a, payload_a);
    harness.wait_for_tx_used().await;

    // Send data on connection B.
    let payload_b = b"connection B data";
    let header_b = harness.guest_header(1001, 6001, Operation::RW, payload_b.len() as u32, 0);
    harness.post_tx_packet(&header_b, payload_b);
    harness.wait_for_tx_used().await;

    // Read data from host side of connection A.
    let mut buf_a = vec![0u8; payload_a.len()];
    stream_a.read_exact(&mut buf_a).unwrap();
    assert_eq!(&buf_a, payload_a);

    // Read data from host side of connection B.
    let mut buf_b = vec![0u8; payload_b.len()];
    stream_b.read_exact(&mut buf_b).unwrap();
    assert_eq!(&buf_b, payload_b);
}

/// Multiple connections to the same host port from different guest ports.
#[async_test]
async fn multiple_connections_same_host_port(driver: DefaultDriver) {
    let tmp_dir = tempfile::tempdir().unwrap();
    let mut harness = TestHarness::new(&driver, tmp_dir);
    let mut listener = harness.create_port_listener(7000);
    harness.enable().await;

    // Connection 1: guest port 2000 -> host port 7000.
    let mut stream1 = harness
        .connect_guest_to_host(&mut listener, 2000, 7000)
        .await;

    // Connection 2: guest port 2001 -> host port 7000.
    let mut stream2 = harness
        .connect_guest_to_host(&mut listener, 2001, 7000)
        .await;

    // Send distinct data on each connection.
    let payload1 = b"stream one";
    let hdr1 = harness.guest_header(2000, 7000, Operation::RW, payload1.len() as u32, 0);
    harness.post_tx_packet(&hdr1, payload1);
    harness.wait_for_tx_used().await;

    let payload2 = b"stream two";
    let hdr2 = harness.guest_header(2001, 7000, Operation::RW, payload2.len() as u32, 0);
    harness.post_tx_packet(&hdr2, payload2);
    harness.wait_for_tx_used().await;

    // Verify each stream received the correct data.
    let mut buf1 = vec![0u8; payload1.len()];
    stream1.read_exact(&mut buf1).unwrap();
    assert_eq!(&buf1, payload1);

    let mut buf2 = vec![0u8; payload2.len()];
    stream2.read_exact(&mut buf2).unwrap();
    assert_eq!(&buf2, payload2);
}

/// Guest-initiated graceful shutdown (both send and receive).
#[async_test]
async fn guest_shutdown(driver: DefaultDriver) {
    let tmp_dir = tempfile::tempdir().unwrap();
    let mut harness = TestHarness::new(&driver, tmp_dir);
    let mut listener = harness.create_port_listener(5010);
    harness.enable().await;

    let mut stream = harness
        .connect_guest_to_host(&mut listener, 1024, 5010)
        .await;

    // Guest sends SHUTDOWN with both send and receive flags.
    let flags = ShutdownFlags::new().with_send(true).with_receive(true);
    let header = harness.guest_header(1024, 5010, Operation::SHUTDOWN, 0, flags.into());
    harness.post_tx_packet(&header, &[]);
    harness.wait_for_tx_used().await;

    // The host socket should see EOF when reading.
    let mut buf = [0u8; 64];
    // Give the device a moment to process.
    std::thread::sleep(Duration::from_millis(50));
    let n = stream.read(&mut buf).unwrap();
    assert_eq!(n, 0, "expected EOF after guest shutdown");
}

/// Sending a large payload (multiple KB) from guest to host.
#[async_test]
async fn large_payload_guest_to_host(driver: DefaultDriver) {
    let tmp_dir = tempfile::tempdir().unwrap();
    let mut harness = TestHarness::new(&driver, tmp_dir);
    let mut listener = harness.create_port_listener(5020);
    harness.enable().await;

    let mut stream = harness
        .connect_guest_to_host(&mut listener, 1024, 5020)
        .await;

    // Send a 4KB payload from the guest.
    let payload: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    let header = harness.guest_header(1024, 5020, Operation::RW, payload.len() as u32, 0);
    harness.post_tx_packet(&header, &payload);
    harness.wait_for_tx_used().await;

    // Read all data on the host side.
    let mut received = vec![0u8; payload.len()];
    stream.read_exact(&mut received).unwrap();
    assert_eq!(received, payload);
}

/// Host-initiated connection: a host Unix client connects to the device's
/// listener, sends `CONNECT <port>\n`, the device forwards a REQUEST to the
/// guest, the guest responds with RESPONSE, and data flows bidirectionally.
#[async_test]
async fn host_connect_to_guest(driver: DefaultDriver) {
    use unix_socket::UnixStream as StdUnixStream;

    let tmp_dir = tempfile::tempdir().unwrap();
    let host_listener_path = tmp_dir.path().join("host_listener.sock");
    let mut harness = TestHarness::new(&driver, tmp_dir);
    harness.enable().await;

    // Post rx buffers so the device can deliver the REQUEST to the guest.
    let mut rx_gpas = Vec::new();
    for _ in 0..4 {
        let (_desc, gpa) = harness.post_rx_buffer(HDR_SIZE + 4096);
        rx_gpas.push(gpa);
    }

    // Host connects to the device's listener socket.
    let mut host_stream =
        StdUnixStream::connect(&host_listener_path).expect("connect to host listener");
    host_stream.set_nonblocking(false).unwrap();

    // Host sends the hybrid vsock connect request for guest port 8080.
    host_stream
        .write_all(b"CONNECT 8080\n")
        .expect("send connect request");

    // Wait for the device to deliver a REQUEST to the guest on the rx queue.
    // Skip any non-REQUEST packets.
    let mut rx_buf_idx = 0;
    let (req_local_port, req_peer_port) = loop {
        let (_rx_id, _rx_len) = harness.wait_for_rx_used().await;
        let gpa = rx_gpas[rx_buf_idx];
        rx_buf_idx += 1;

        let hdr = harness.read_header(gpa);
        let op = hdr.op;
        if Operation(op) == Operation::REQUEST {
            let src_port = hdr.src_port;
            let dst_port = hdr.dst_port;
            let dst_cid = hdr.dst_cid;
            assert_eq!(dst_cid, GUEST_CID);
            assert_eq!(dst_port, 8080);
            break (src_port, dst_port);
        }
    };

    // Guest responds with RESPONSE to accept the connection.
    let response_hdr = VsockHeader {
        src_cid: GUEST_CID,
        dst_cid: VSOCK_CID_HOST,
        src_port: req_peer_port,
        dst_port: req_local_port,
        len: 0,
        socket_type: SocketType::STREAM.0,
        op: Operation::RESPONSE.0,
        flags: 0,
        buf_alloc: GUEST_BUF_ALLOC,
        fwd_cnt: 0,
    };
    harness.post_tx_packet(&response_hdr, &[]);
    harness.wait_for_tx_used().await;

    // The device should send "OK <port>\n" back through the Unix socket.
    let mut ok_buf = [0u8; 64];
    let n = host_stream.read(&mut ok_buf).expect("read OK response");
    let ok_str = std::str::from_utf8(&ok_buf[..n]).expect("valid UTF-8");
    let ok_str = ok_str.trim_end_matches('\n');
    let port_str = ok_str
        .strip_prefix("OK ")
        .unwrap_or_else(|| panic!("expected 'OK <port>', got: {ok_str:?}"));
    let ok_port: u32 = port_str
        .parse()
        .unwrap_or_else(|_| panic!("expected numeric port in OK response, got: {port_str:?}"));
    assert!(ok_port > 0, "OK port should be non-zero, got {ok_port}");

    // Now the connection is established. Send data from the host to the guest.
    let payload = b"hello from host side";
    host_stream.write_all(payload).expect("write host data");

    // Wait for RW packet on the rx queue.
    loop {
        let (_rx_id, rx_len) = harness.wait_for_rx_used().await;
        let gpa = rx_gpas[rx_buf_idx];
        rx_buf_idx += 1;

        let hdr = harness.read_header(gpa);
        let op = hdr.op;
        if Operation(op) == Operation::RW {
            let data_len = hdr.len;
            assert!(rx_len >= HDR_SIZE + data_len);
            let data = harness.read_rx_data(gpa, data_len);
            assert_eq!(&data, payload);
            break;
        }
    }

    // Send data from the guest to the host through the same connection.
    let guest_payload = b"hello from guest side";
    let rw_hdr = harness.guest_header(
        req_peer_port,
        req_local_port,
        Operation::RW,
        guest_payload.len() as u32,
        0,
    );
    harness.post_tx_packet(&rw_hdr, guest_payload);
    harness.wait_for_tx_used().await;

    // Read the data on the host side.
    let mut recv_buf = vec![0u8; guest_payload.len()];
    host_stream
        .read_exact(&mut recv_buf)
        .expect("read guest data on host");
    assert_eq!(&recv_buf, guest_payload);
}

/// Host-initiated connection using an hvsocket vsock template GUID in the
/// CONNECT message. The device should parse the GUID, extract the embedded
/// port, deliver a REQUEST to the guest, and reply with an OK containing the
/// same GUID format.
#[async_test]
async fn host_connect_to_guest_with_guid(driver: DefaultDriver) {
    use unix_socket::UnixStream as StdUnixStream;

    let tmp_dir = tempfile::tempdir().unwrap();
    let host_listener_path = tmp_dir.path().join("host_listener.sock");
    let mut harness = TestHarness::new(&driver, tmp_dir);
    harness.enable().await;

    // Post rx buffers.
    let mut rx_gpas = Vec::new();
    for _ in 0..4 {
        let (_desc, gpa) = harness.post_rx_buffer(HDR_SIZE + 4096);
        rx_gpas.push(gpa);
    }

    // Host connects to the device's listener socket.
    let mut host_stream =
        StdUnixStream::connect(&host_listener_path).expect("connect to host listener");
    host_stream.set_nonblocking(false).unwrap();

    // Send CONNECT using the hvsocket vsock template GUID for port 8080.
    // Port 8080 = 0x1F90, so the GUID is 00001f90-facb-11e6-bd58-64006a7986d3.
    let connect_guid = "00001f90-facb-11e6-bd58-64006a7986d3";
    let connect_msg = format!("CONNECT {connect_guid}\n");
    host_stream
        .write_all(connect_msg.as_bytes())
        .expect("send GUID connect request");

    // Wait for the REQUEST on the rx queue.
    let mut rx_buf_idx = 0;
    let (req_local_port, req_peer_port) = loop {
        let (_rx_id, _rx_len) = harness.wait_for_rx_used().await;
        let gpa = rx_gpas[rx_buf_idx];
        rx_buf_idx += 1;

        let hdr = harness.read_header(gpa);
        let op = hdr.op;
        if Operation(op) == Operation::REQUEST {
            let src_port = hdr.src_port;
            let dst_port = hdr.dst_port;
            let dst_cid = hdr.dst_cid;
            assert_eq!(dst_cid, GUEST_CID);
            // The device should extract port 8080 from the GUID.
            assert_eq!(dst_port, 8080);
            break (src_port, dst_port);
        }
    };

    // Guest responds with RESPONSE.
    let response_hdr = VsockHeader {
        src_cid: GUEST_CID,
        dst_cid: VSOCK_CID_HOST,
        src_port: req_peer_port,
        dst_port: req_local_port,
        len: 0,
        socket_type: SocketType::STREAM.0,
        op: Operation::RESPONSE.0,
        flags: 0,
        buf_alloc: GUEST_BUF_ALLOC,
        fwd_cnt: 0,
    };
    harness.post_tx_packet(&response_hdr, &[]);
    harness.wait_for_tx_used().await;

    // The device should reply with "OK <guid>\n" using the same GUID format.
    let mut ok_buf = [0u8; 128];
    let n = host_stream.read(&mut ok_buf).expect("read OK response");
    let ok_str = std::str::from_utf8(&ok_buf[..n]).expect("valid UTF-8");
    let ok_str = ok_str.trim_end_matches('\n');
    let ok_value = ok_str
        .strip_prefix("OK ")
        .unwrap_or_else(|| panic!("expected 'OK <guid>', got: {ok_str:?}"));

    // Verify the response is a valid GUID matching the vsock template for
    // the device's local port.
    let ok_guid: guid::Guid = ok_value
        .parse()
        .unwrap_or_else(|_| panic!("expected GUID in OK response, got: {ok_value:?}"));

    // The GUID should use the vsock template with the local port embedded.
    let ok_port = hybrid_vsock::VsockPortOrId::Id(ok_guid)
        .port()
        .unwrap_or_else(|| panic!("OK GUID does not match vsock template: {ok_guid}"));
    assert!(ok_port > 0, "OK port should be non-zero, got {ok_port}");
}

/// Two simultaneous host-to-guest connections must receive different local
/// port numbers in their OK responses. This exercises port allocation —
/// currently the device hardcodes local_port to 1234, so this test is
/// expected to fail until that is fixed.
#[async_test]
async fn host_connect_two_connections_get_different_ports(driver: DefaultDriver) {
    use unix_socket::UnixStream as StdUnixStream;

    let tmp_dir = tempfile::tempdir().unwrap();
    let host_listener_path = tmp_dir.path().join("host_listener.sock");
    let mut harness = TestHarness::new(&driver, tmp_dir);
    harness.enable().await;

    // Post plenty of rx buffers for both connection handshakes.
    let mut rx_gpas = Vec::new();
    for _ in 0..8 {
        let (_desc, gpa) = harness.post_rx_buffer(HDR_SIZE + 4096);
        rx_gpas.push(gpa);
    }
    let mut rx_buf_idx = 0;

    // Helper: drive one host-to-guest connection through the handshake and
    // return the port number from the OK response.
    //
    // Because we can't call async methods on harness from a closure (borrow
    // issues), the logic is inlined below for each connection.

    // --- Connection 1 ---
    let mut host_stream1 =
        StdUnixStream::connect(&host_listener_path).expect("connect to host listener (1)");
    host_stream1.set_nonblocking(false).unwrap();
    host_stream1
        .write_all(b"CONNECT 9001\n")
        .expect("send connect (1)");

    // Wait for REQUEST for connection 1.
    let (req1_local, req1_peer) = loop {
        let (_rx_id, _rx_len) = harness.wait_for_rx_used().await;
        let gpa = rx_gpas[rx_buf_idx];
        rx_buf_idx += 1;
        let hdr = harness.read_header(gpa);
        if Operation(hdr.op) == Operation::REQUEST {
            let dst_cid = hdr.dst_cid;
            let dst_port = hdr.dst_port;
            assert_eq!(dst_cid, GUEST_CID);
            assert_eq!(dst_port, 9001);
            break (hdr.src_port, dst_port);
        }
    };

    // Guest accepts connection 1.
    let resp1 = VsockHeader {
        src_cid: GUEST_CID,
        dst_cid: VSOCK_CID_HOST,
        src_port: req1_peer,
        dst_port: req1_local,
        len: 0,
        socket_type: SocketType::STREAM.0,
        op: Operation::RESPONSE.0,
        flags: 0,
        buf_alloc: GUEST_BUF_ALLOC,
        fwd_cnt: 0,
    };
    harness.post_tx_packet(&resp1, &[]);
    harness.wait_for_tx_used().await;

    // Read OK for connection 1.
    let mut ok_buf = [0u8; 64];
    let n = host_stream1.read(&mut ok_buf).expect("read OK (1)");
    let ok1 = std::str::from_utf8(&ok_buf[..n])
        .expect("valid UTF-8")
        .trim_end_matches('\n')
        .strip_prefix("OK ")
        .unwrap_or_else(|| panic!("expected OK, got: {:?}", &ok_buf[..n]))
        .to_string();
    let port1: u32 = ok1
        .parse()
        .unwrap_or_else(|_| panic!("expected numeric port, got: {ok1:?}"));

    // --- Connection 2 ---
    let mut host_stream2 =
        StdUnixStream::connect(&host_listener_path).expect("connect to host listener (2)");
    host_stream2.set_nonblocking(false).unwrap();
    host_stream2
        .write_all(b"CONNECT 9002\n")
        .expect("send connect (2)");

    // Wait for REQUEST for connection 2.
    let (req2_local, req2_peer) = loop {
        let (_rx_id, _rx_len) = harness.wait_for_rx_used().await;
        let gpa = rx_gpas[rx_buf_idx];
        rx_buf_idx += 1;
        let hdr = harness.read_header(gpa);
        if Operation(hdr.op) == Operation::REQUEST {
            let dst_cid = hdr.dst_cid;
            let dst_port = hdr.dst_port;
            assert_eq!(dst_cid, GUEST_CID);
            assert_eq!(dst_port, 9002);
            break (hdr.src_port, dst_port);
        }
    };

    // Guest accepts connection 2.
    let resp2 = VsockHeader {
        src_cid: GUEST_CID,
        dst_cid: VSOCK_CID_HOST,
        src_port: req2_peer,
        dst_port: req2_local,
        len: 0,
        socket_type: SocketType::STREAM.0,
        op: Operation::RESPONSE.0,
        flags: 0,
        buf_alloc: GUEST_BUF_ALLOC,
        fwd_cnt: 0,
    };
    harness.post_tx_packet(&resp2, &[]);
    harness.wait_for_tx_used().await;

    // Read OK for connection 2.
    let n = host_stream2.read(&mut ok_buf).expect("read OK (2)");
    let ok2 = std::str::from_utf8(&ok_buf[..n])
        .expect("valid UTF-8")
        .trim_end_matches('\n')
        .strip_prefix("OK ")
        .unwrap_or_else(|| panic!("expected OK, got: {:?}", &ok_buf[..n]))
        .to_string();
    let port2: u32 = ok2
        .parse()
        .unwrap_or_else(|_| panic!("expected numeric port, got: {ok2:?}"));

    // The two connections must have been assigned different local ports.
    assert_ne!(
        port1, port2,
        "two simultaneous host connections should get different ports, but both got {port1}"
    );
}

/// Exercises the device's internal ring buffer by making the host stop
/// reading so the Unix socket back-pressures, then resuming reads and
/// verifying all data arrives intact and that credit updates flow.
///
/// Flow:
///  1. Establish a guest→host connection.
///  2. Shrink the host socket's receive buffer so back-pressure builds
///     quickly.
///  3. Guest sends many small RW packets without the host reading.
///  4. Eventually the device's socket write blocks and data accumulates in
///     the device's 64 KB ring buffer.
///  5. Host begins draining data — the device flushes its buffer and sends
///     CREDIT_UPDATE packets on the rx queue as `fwd_cnt` advances.
///  6. Assert that all bytes arrive and at least one CREDIT_UPDATE was seen.
#[async_test]
async fn guest_send_exercises_ring_buffer(driver: DefaultDriver) {
    let tmp_dir = tempfile::tempdir().unwrap();
    let mut harness = TestHarness::new(&driver, tmp_dir);
    let mut listener = harness.create_port_listener(5050);
    harness.enable().await;

    let stream = harness
        .connect_guest_to_host(&mut listener, 1024, 5050)
        .await;

    // Send data from the guest in 1 KB chunks. We'll send enough to exceed the socket buffer and
    // spill into the device's ring buffer. On Windows, this seems to require about 64KB, and on
    // Linux it's around 96KB.
    const CHUNK_SIZE: usize = 1024;
    const NUM_CHUNKS: usize = 128;
    const TOTAL_BYTES: usize = CHUNK_SIZE * NUM_CHUNKS;

    let mut sent_bytes = Vec::with_capacity(TOTAL_BYTES);
    for i in 0..NUM_CHUNKS {
        let chunk: Vec<u8> = (0..CHUNK_SIZE)
            .map(|j| ((i * 7 + j * 3) % 251) as u8)
            .collect();
        let header = harness.guest_header(1024, 5050, Operation::RW, chunk.len() as u32, 0);
        // Reuse descriptor 0 and the same data region each iteration since
        // we wait for the previous one to complete before posting the next.
        harness.next_tx_desc = 0;
        harness.next_data_offset = DATA_BASE;
        harness.post_tx_packet(&header, &chunk);
        // Wait for the tx descriptor to be consumed so the device processes
        // each packet before we queue the next.
        harness.wait_for_tx_used().await;
        sent_bytes.extend_from_slice(&chunk);
    }

    // Post an RX buffer so the device can send a credit update, which should be pending since it
    // eagerly sends them.
    let (_desc, gpa) = harness.post_rx_buffer(HDR_SIZE + 4096);
    let (_rx_id, _rx_len) = harness.wait_for_rx_used().await;
    let hdr = harness.read_header(gpa);
    let op = hdr.op;
    assert_eq!(Operation(op), Operation::CREDIT_UPDATE);
    assert!(
        hdr.fwd_cnt > 0,
        "device should have forwarded at least some data."
    );
    assert!(
        hdr.fwd_cnt < TOTAL_BYTES as u32,
        "some data should have been buffered"
    );

    // Host begins reading — this unblocks the device's socket writes and
    // triggers ring buffer flushes + credit updates.
    let mut received = Vec::with_capacity(TOTAL_BYTES);
    let mut buf = [0u8; 4096];
    // Use non-blocking reads in a poll loop so the async executor can
    // process the device's write-ready events concurrently.
    let mut polled_stream = PolledSocket::new(&driver, stream).unwrap();
    mesh::CancelContext::new()
        .with_timeout(Duration::from_secs(10))
        .until_cancelled(async {
            use futures::AsyncReadExt;
            while received.len() < TOTAL_BYTES {
                let n = polled_stream
                    .read(&mut buf)
                    .await
                    .expect("host read failed");
                assert!(n > 0, "unexpected EOF from host socket");
                received.extend_from_slice(&buf[..n]);
            }
        })
        .await
        .expect("timed out reading data on host side");

    assert_eq!(received.len(), TOTAL_BYTES);
    assert_eq!(
        received, sent_bytes,
        "received data does not match sent data"
    );

    // Post another RX buffer so we can get the final credit update.
    let (_desc, gpa) = harness.post_rx_buffer(HDR_SIZE + 4096);
    let (_rx_id, _rx_len) = harness.wait_for_rx_used().await;
    let hdr = harness.read_header(gpa);
    let op = hdr.op;
    assert_eq!(Operation(op), Operation::CREDIT_UPDATE);
    let fwd_cnt = hdr.fwd_cnt;
    assert_eq!(
        fwd_cnt, TOTAL_BYTES as u32,
        "final credit update should reflect all forwarded data"
    );
}
