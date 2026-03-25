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
use std::os::unix::net::UnixListener as StdUnixListener;
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

const QUEUE_SIZE: u16 = 32;
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
const TOTAL_MEM_SIZE: usize = 0x80000;

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
    fn create_port_listener(&self, port: u32) -> StdUnixListener {
        let socket_path = self._tmp_dir.path().join(format!("vsock_{port}"));
        StdUnixListener::bind(&socket_path).expect("bind port listener")
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
        listener: &StdUnixListener,
        guest_port: u32,
        host_port: u32,
    ) -> std::os::unix::net::UnixStream {
        // Post an rx buffer for the response.
        let (_rx_desc, rx_gpa) = self.post_rx_buffer(HDR_SIZE + 1024);

        // Send the connection request.
        let header = self.guest_header(guest_port, host_port, Operation::REQUEST, 0, 0);
        self.post_tx_packet(&header, &[]);

        // Accept the connection on the host side using async polling so the
        // device worker can make progress on the same async executor.
        let mut polled_listener =
            PolledSocket::new(&self.driver, listener.try_clone().unwrap()).unwrap();
        let (stream, _) = mesh::CancelContext::new()
            .with_timeout(Duration::from_secs(5))
            .until_cancelled(polled_listener.accept())
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
    let listener = harness.create_port_listener(5000);
    harness.enable().await;

    let _stream = harness.connect_guest_to_host(&listener, 1024, 5000).await;
}

/// Guest sends data to the host through an established connection.
#[async_test]
async fn guest_to_host_data(driver: DefaultDriver) {
    let tmp_dir = tempfile::tempdir().unwrap();
    let mut harness = TestHarness::new(&driver, tmp_dir);
    let listener = harness.create_port_listener(5001);
    harness.enable().await;

    let mut stream = harness.connect_guest_to_host(&listener, 1024, 5001).await;

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
    let listener = harness.create_port_listener(5002);
    harness.enable().await;

    let mut stream = harness.connect_guest_to_host(&listener, 1024, 5002).await;

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
    let listener = harness.create_port_listener(5003);
    harness.enable().await;

    let mut stream = harness.connect_guest_to_host(&listener, 1024, 5003).await;

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
    let listener_a = harness.create_port_listener(6000);
    let listener_b = harness.create_port_listener(6001);
    harness.enable().await;

    // Establish connection A (guest port 1000 -> host port 6000).
    let mut stream_a = harness.connect_guest_to_host(&listener_a, 1000, 6000).await;

    // Establish connection B (guest port 1001 -> host port 6001).
    let mut stream_b = harness.connect_guest_to_host(&listener_b, 1001, 6001).await;

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
    let listener = harness.create_port_listener(7000);
    harness.enable().await;

    // Connection 1: guest port 2000 -> host port 7000.
    let mut stream1 = harness.connect_guest_to_host(&listener, 2000, 7000).await;

    // Connection 2: guest port 2001 -> host port 7000.
    let mut stream2 = harness.connect_guest_to_host(&listener, 2001, 7000).await;

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
    let listener = harness.create_port_listener(5010);
    harness.enable().await;

    let mut stream = harness.connect_guest_to_host(&listener, 1024, 5010).await;

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
    let listener = harness.create_port_listener(5020);
    harness.enable().await;

    let mut stream = harness.connect_guest_to_host(&listener, 1024, 5020).await;

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
