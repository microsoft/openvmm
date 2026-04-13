// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the virtio-blk device.
//!
//! These tests construct a full `VirtioBlkDevice` with guest memory, a RAM
//! disk backend, and real virtio queues — then drive requests through the
//! descriptor ring just as a guest driver would.

use crate::VirtioBlkDevice;
use disk_backend::Disk;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use guestmem::GuestMemory;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use inspect::Inspect;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_event::Event;
use parking_lot::Mutex;
use scsi_buffers::RequestBuffers;
use test_with_tracing::test;
use virtio::QueueResources;
use virtio::VirtioDevice;
use virtio::queue::QueueParams;
use virtio::spec::VirtioDeviceFeatures;
use virtio::spec::blk::VirtioBlkDiscardWriteZeroes;
use virtio::spec::blk::*;
use virtio::spec::queue::DescriptorFlags;
use virtio::test_helpers::init_avail_ring;
use virtio::test_helpers::init_used_ring;
use virtio::test_helpers::make_available;
use virtio::test_helpers::wait_for_used;
use virtio::test_helpers::write_descriptor;
use vmcore::interrupt::Interrupt;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::IntoBytes;

// --- Constants ---

const QUEUE_SIZE: u16 = 32;

// Memory layout for the single requestq
const DESC_ADDR: u64 = 0x0000;
const AVAIL_ADDR: u64 = 0x1000;
const USED_ADDR: u64 = 0x2000;

// Data area for request headers, payloads, and status bytes
const DATA_BASE: u64 = 0x10000;
const TOTAL_MEM_SIZE: usize = 0x40000;

// VirtioBlkReqHeader is 16 bytes (u32 type, u32 reserved, u64 sector)
const REQ_HEADER_SIZE: u32 = 16;

// --- Test Harness ---

struct TestHarness {
    device: VirtioBlkDevice,
    mem: GuestMemory,
    driver: DefaultDriver,
    queue_event: Event,
    interrupt_event: Event,
    avail_idx: u16,
    used_idx: u16,
    next_data_offset: u64,
}

impl TestHarness {
    /// Create a harness with a RAM disk of the given size.
    fn new(driver: &DefaultDriver, disk: Disk, read_only: bool) -> Self {
        let mem = GuestMemory::allocate(TOTAL_MEM_SIZE);

        init_avail_ring(&mem, AVAIL_ADDR);
        init_used_ring(&mem, USED_ADDR);

        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
        let device = VirtioBlkDevice::new(&driver_source, disk, read_only);

        let queue_event = Event::new();
        let interrupt_event = Event::new();

        Self {
            device,
            mem,
            driver: driver.clone(),
            queue_event,
            interrupt_event,
            avail_idx: 0,
            used_idx: 0,
            next_data_offset: DATA_BASE,
        }
    }

    /// Enable the device with one queue.
    async fn enable(&mut self) {
        let interrupt = Interrupt::from_event(self.interrupt_event.clone());

        self.device
            .start_queue(
                0,
                QueueResources {
                    params: QueueParams {
                        size: QUEUE_SIZE,
                        enable: true,
                        desc_addr: DESC_ADDR,
                        avail_addr: AVAIL_ADDR,
                        used_addr: USED_ADDR,
                    },
                    notify: interrupt,
                    event: self.queue_event.clone(),
                    guest_memory: self.mem.clone(),
                },
                &VirtioDeviceFeatures::new(),
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

    /// Build a read request descriptor chain.
    ///
    /// Layout (per virtio-blk spec §5.2.6):
    ///   desc 0 (readable): VirtioBlkReqHeader { type=IN, sector }
    ///   desc 1 (writable): data buffer (data_len bytes) + 1 status byte
    ///
    /// Returns the head descriptor index.
    fn post_read_request(&mut self, head_desc: u16, sector: u64, data_len: u32) -> u64 {
        let header_gpa = self.alloc_data(REQ_HEADER_SIZE);
        let data_gpa = self.alloc_data(data_len + 1); // +1 for status byte

        // Write the request header
        let header = VirtioBlkReqHeader {
            request_type: VIRTIO_BLK_T_IN,
            reserved: 0,
            sector,
        };
        self.mem.write_at(header_gpa, header.as_bytes()).unwrap();

        // Zero the data+status buffer
        let zeroes = vec![0u8; (data_len + 1) as usize];
        self.mem.write_at(data_gpa, &zeroes).unwrap();

        // desc 0: header (readable)
        let flags0 = DescriptorFlags::new().with_next(true);
        write_descriptor(
            &self.mem,
            DESC_ADDR,
            head_desc,
            header_gpa,
            REQ_HEADER_SIZE,
            flags0,
            head_desc + 1,
        );

        // desc 1: data + status (writable)
        let flags1 = DescriptorFlags::new().with_write(true);
        write_descriptor(
            &self.mem,
            DESC_ADDR,
            head_desc + 1,
            data_gpa,
            data_len + 1,
            flags1,
            0,
        );

        make_available(
            &self.mem,
            AVAIL_ADDR,
            QUEUE_SIZE,
            head_desc,
            &mut self.avail_idx,
        );
        self.queue_event.signal();

        data_gpa
    }

    /// Build a write request descriptor chain.
    ///
    /// Layout (per virtio-blk spec §5.2.6):
    ///   desc 0 (readable): VirtioBlkReqHeader { type=OUT, sector }
    ///   desc 1 (readable): data to write
    ///   desc 2 (writable): 1-byte status
    ///
    /// Returns the head descriptor index.
    fn post_write_request(&mut self, head_desc: u16, sector: u64, data: &[u8]) {
        let header_gpa = self.alloc_data(REQ_HEADER_SIZE);
        let data_gpa = self.alloc_data(data.len() as u32);
        let status_gpa = self.alloc_data(1);

        // Write the request header
        let header = VirtioBlkReqHeader {
            request_type: VIRTIO_BLK_T_OUT,
            reserved: 0,
            sector,
        };
        self.mem.write_at(header_gpa, header.as_bytes()).unwrap();

        // Write the data payload
        self.mem.write_at(data_gpa, data).unwrap();

        // Zero the status byte
        self.mem.write_at(status_gpa, &[0u8]).unwrap();

        // desc 0: header (readable)
        let flags0 = DescriptorFlags::new().with_next(true);
        write_descriptor(
            &self.mem,
            DESC_ADDR,
            head_desc,
            header_gpa,
            REQ_HEADER_SIZE,
            flags0,
            head_desc + 1,
        );

        // desc 1: data (readable)
        let flags1 = DescriptorFlags::new().with_next(true);
        write_descriptor(
            &self.mem,
            DESC_ADDR,
            head_desc + 1,
            data_gpa,
            data.len() as u32,
            flags1,
            head_desc + 2,
        );

        // desc 2: status (writable)
        let flags2 = DescriptorFlags::new().with_write(true);
        write_descriptor(
            &self.mem,
            DESC_ADDR,
            head_desc + 2,
            status_gpa,
            1,
            flags2,
            0,
        );

        make_available(
            &self.mem,
            AVAIL_ADDR,
            QUEUE_SIZE,
            head_desc,
            &mut self.avail_idx,
        );
        self.queue_event.signal();
    }

    /// Build a flush request descriptor chain.
    fn post_flush_request(&mut self, head_desc: u16) {
        let header_gpa = self.alloc_data(REQ_HEADER_SIZE);
        let status_gpa = self.alloc_data(1);

        let header = VirtioBlkReqHeader {
            request_type: VIRTIO_BLK_T_FLUSH,
            reserved: 0,
            sector: 0,
        };
        self.mem.write_at(header_gpa, header.as_bytes()).unwrap();
        self.mem.write_at(status_gpa, &[0xFFu8]).unwrap();

        // desc 0: header (readable)
        let flags0 = DescriptorFlags::new().with_next(true);
        write_descriptor(
            &self.mem,
            DESC_ADDR,
            head_desc,
            header_gpa,
            REQ_HEADER_SIZE,
            flags0,
            head_desc + 1,
        );

        // desc 1: status (writable)
        let flags1 = DescriptorFlags::new().with_write(true);
        write_descriptor(
            &self.mem,
            DESC_ADDR,
            head_desc + 1,
            status_gpa,
            1,
            flags1,
            0,
        );

        make_available(
            &self.mem,
            AVAIL_ADDR,
            QUEUE_SIZE,
            head_desc,
            &mut self.avail_idx,
        );
        self.queue_event.signal();
    }

    /// Build a get-id request descriptor chain.
    fn post_get_id_request(&mut self, head_desc: u16) -> u64 {
        let header_gpa = self.alloc_data(REQ_HEADER_SIZE);
        let id_gpa = self.alloc_data(VIRTIO_BLK_ID_BYTES as u32 + 1); // id + status

        let header = VirtioBlkReqHeader {
            request_type: VIRTIO_BLK_T_GET_ID,
            reserved: 0,
            sector: 0,
        };
        self.mem.write_at(header_gpa, header.as_bytes()).unwrap();
        self.mem
            .write_at(id_gpa, &[0u8; VIRTIO_BLK_ID_BYTES + 1])
            .unwrap();

        // desc 0: header (readable)
        let flags0 = DescriptorFlags::new().with_next(true);
        write_descriptor(
            &self.mem,
            DESC_ADDR,
            head_desc,
            header_gpa,
            REQ_HEADER_SIZE,
            flags0,
            head_desc + 1,
        );

        // desc 1: id + status (writable)
        let flags1 = DescriptorFlags::new().with_write(true);
        write_descriptor(
            &self.mem,
            DESC_ADDR,
            head_desc + 1,
            id_gpa,
            VIRTIO_BLK_ID_BYTES as u32 + 1,
            flags1,
            0,
        );

        make_available(
            &self.mem,
            AVAIL_ADDR,
            QUEUE_SIZE,
            head_desc,
            &mut self.avail_idx,
        );
        self.queue_event.signal();

        id_gpa
    }

    /// Build a discard request descriptor chain.
    ///
    /// Layout (per virtio-blk spec §5.2.6):
    ///   desc 0 (readable): VirtioBlkReqHeader { type=DISCARD, sector=0 }
    ///                       + VirtioBlkDiscardWriteZeroes { sector, num_sectors, flags }
    ///   desc 1 (writable): 1-byte status
    ///
    /// Returns the status byte GPA.
    fn post_discard_request(
        &mut self,
        head_desc: u16,
        discard_sector: u64,
        num_sectors: u32,
        flags: u32,
    ) -> u64 {
        // Combined header + discard segment = 16 + 16 = 32 bytes
        let req_gpa = self.alloc_data(32);
        let status_gpa = self.alloc_data(1);

        // Write the header (type=DISCARD, sector field unused for discard)
        let header = VirtioBlkReqHeader {
            request_type: VIRTIO_BLK_T_DISCARD,
            reserved: 0,
            sector: 0,
        };
        self.mem.write_at(req_gpa, header.as_bytes()).unwrap();

        // Write the discard segment immediately after the header
        let seg = VirtioBlkDiscardWriteZeroes {
            sector: discard_sector,
            num_sectors,
            flags,
        };
        self.mem
            .write_at(req_gpa + REQ_HEADER_SIZE as u64, seg.as_bytes())
            .unwrap();
        self.mem.write_at(status_gpa, &[0xFFu8]).unwrap();

        // desc 0: header + segment (readable)
        let flags0 = DescriptorFlags::new().with_next(true);
        write_descriptor(
            &self.mem,
            DESC_ADDR,
            head_desc,
            req_gpa,
            32,
            flags0,
            head_desc + 1,
        );

        // desc 1: status (writable)
        let flags1 = DescriptorFlags::new().with_write(true);
        write_descriptor(
            &self.mem,
            DESC_ADDR,
            head_desc + 1,
            status_gpa,
            1,
            flags1,
            0,
        );

        make_available(
            &self.mem,
            AVAIL_ADDR,
            QUEUE_SIZE,
            head_desc,
            &mut self.avail_idx,
        );
        self.queue_event.signal();

        status_gpa
    }

    /// Build a request with an arbitrary type code (for testing unsupported types).
    fn post_raw_request(&mut self, head_desc: u16, request_type: u32, sector: u64) -> u64 {
        let header_gpa = self.alloc_data(REQ_HEADER_SIZE);
        let status_gpa = self.alloc_data(1);

        let header = VirtioBlkReqHeader {
            request_type,
            reserved: 0,
            sector,
        };
        self.mem.write_at(header_gpa, header.as_bytes()).unwrap();
        self.mem.write_at(status_gpa, &[0xFFu8]).unwrap();

        // desc 0: header (readable)
        let flags0 = DescriptorFlags::new().with_next(true);
        write_descriptor(
            &self.mem,
            DESC_ADDR,
            head_desc,
            header_gpa,
            REQ_HEADER_SIZE,
            flags0,
            head_desc + 1,
        );

        // desc 1: status (writable)
        let flags1 = DescriptorFlags::new().with_write(true);
        write_descriptor(
            &self.mem,
            DESC_ADDR,
            head_desc + 1,
            status_gpa,
            1,
            flags1,
            0,
        );

        make_available(
            &self.mem,
            AVAIL_ADDR,
            QUEUE_SIZE,
            head_desc,
            &mut self.avail_idx,
        );
        self.queue_event.signal();

        status_gpa
    }

    /// Wait for the next used ring entry with a timeout.
    async fn wait_for_used(&mut self) -> (u16, u32) {
        wait_for_used(
            &self.driver,
            &self.interrupt_event,
            &self.mem,
            USED_ADDR,
            QUEUE_SIZE,
            &mut self.used_idx,
        )
        .await
    }

    /// Read a status byte from guest memory at the given GPA.
    fn read_status(&self, status_gpa: u64) -> u8 {
        let mut buf = [0u8; 1];
        self.mem.read_at(status_gpa, &mut buf).unwrap();
        buf[0]
    }
}

// --- Tests ---

fn ram_disk(size: u64, read_only: bool) -> Disk {
    disklayer_ram::ram_disk(size, read_only).unwrap()
}

/// Write 1 sector then read it back. Verifies basic write and read roundtrip.
#[async_test]
async fn write_then_read_roundtrip(driver: DefaultDriver) {
    let disk = ram_disk(64 * 1024, false); // 64 KiB
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.enable().await;

    // Write a recognizable pattern to sector 0.
    let data: Vec<u8> = (0..512).map(|i| (i % 251) as u8).collect();
    harness.post_write_request(0, 0, &data);
    let (used_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_id, 0);
    // used_len = 1 (status byte only for writes)
    assert_eq!(used_len, 1);

    // Read sector 0 back.
    let data_gpa = harness.post_read_request(3, 0, 512);
    let (used_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_id, 3);
    // used_len = 512 data bytes + 1 status byte
    assert_eq!(used_len, 513);

    // Verify the data.
    let mut readback = vec![0u8; 512];
    harness.mem.read_at(data_gpa, &mut readback).unwrap();
    assert_eq!(readback, data, "read-back data mismatch");

    // Verify success status byte (immediately after data).
    let status = harness.read_status(data_gpa + 512);
    assert_eq!(status, VIRTIO_BLK_S_OK);
}

/// Read from a sector that was never written — should succeed with zeroes.
#[async_test]
async fn read_unwritten_sector_returns_zeroes(driver: DefaultDriver) {
    let disk = ram_disk(64 * 1024, false);
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.enable().await;

    let data_gpa = harness.post_read_request(0, 4, 512);
    let (used_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_id, 0);
    assert_eq!(used_len, 513);

    let mut readback = vec![0xFFu8; 512];
    harness.mem.read_at(data_gpa, &mut readback).unwrap();
    assert!(readback.iter().all(|&b| b == 0), "expected all zeroes");
}

/// Write to a read-only disk — should fail with IOERR status.
#[async_test]
async fn write_to_read_only_disk_fails(driver: DefaultDriver) {
    let disk = ram_disk(64 * 1024, true);
    let mut harness = TestHarness::new(&driver, disk, true);
    harness.enable().await;

    // Attempt to write — this should fail.
    // We need to find the status byte location: it's the writable descriptor
    // (desc 2 at status_gpa).
    let header_gpa = harness.alloc_data(REQ_HEADER_SIZE);
    let data_gpa = harness.alloc_data(512);
    let status_gpa = harness.alloc_data(1);

    let header = VirtioBlkReqHeader {
        request_type: VIRTIO_BLK_T_OUT,
        reserved: 0,
        sector: 0,
    };
    harness.mem.write_at(header_gpa, header.as_bytes()).unwrap();
    harness.mem.write_at(data_gpa, &[0xABu8; 512]).unwrap();
    harness.mem.write_at(status_gpa, &[0xFFu8]).unwrap();

    let flags0 = DescriptorFlags::new().with_next(true);
    write_descriptor(
        &harness.mem,
        DESC_ADDR,
        0,
        header_gpa,
        REQ_HEADER_SIZE,
        flags0,
        1,
    );
    let flags1 = DescriptorFlags::new().with_next(true);
    write_descriptor(&harness.mem, DESC_ADDR, 1, data_gpa, 512, flags1, 2);
    let flags2 = DescriptorFlags::new().with_write(true);
    write_descriptor(&harness.mem, DESC_ADDR, 2, status_gpa, 1, flags2, 0);

    make_available(
        &harness.mem,
        AVAIL_ADDR,
        QUEUE_SIZE,
        0,
        &mut harness.avail_idx,
    );
    harness.queue_event.signal();

    let (used_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_id, 0);
    assert_eq!(used_len, 1); // just the status byte

    let status = harness.read_status(status_gpa);
    assert_eq!(status, VIRTIO_BLK_S_IOERR);
}

/// Flush command should succeed.
#[async_test]
async fn flush_succeeds(driver: DefaultDriver) {
    let disk = ram_disk(64 * 1024, false);
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.enable().await;

    harness.post_flush_request(0);
    let (used_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_id, 0);
    assert_eq!(used_len, 1); // status byte only
}

/// GET_ID request should return a device identifier string.
#[async_test]
async fn get_id_returns_identifier(driver: DefaultDriver) {
    let disk = ram_disk(64 * 1024, false);
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.enable().await;

    let id_gpa = harness.post_get_id_request(0);
    let (used_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_id, 0);
    // used_len = 20 (id bytes) + 1 (status byte)
    assert_eq!(used_len, VIRTIO_BLK_ID_BYTES as u32 + 1);

    // Verify the ID is the default "openvmm-virtio-blk\0\0"
    let mut id_buf = [0u8; VIRTIO_BLK_ID_BYTES];
    harness.mem.read_at(id_gpa, &mut id_buf).unwrap();
    assert_eq!(&id_buf, b"openvmm-virtio-blk\0\0");
}

/// Unsupported request type should return UNSUPP status.
#[async_test]
async fn unsupported_request_type(driver: DefaultDriver) {
    let disk = ram_disk(64 * 1024, false);
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.enable().await;

    let status_gpa = harness.post_raw_request(0, 0xFF, 0);
    let (used_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_id, 0);
    assert_eq!(used_len, 1);

    let status = harness.read_status(status_gpa);
    assert_eq!(status, VIRTIO_BLK_S_UNSUPP);
}

/// Write to multiple sectors then read them back to verify multi-sector IO.
#[async_test]
async fn multi_sector_write_read(driver: DefaultDriver) {
    let disk = ram_disk(64 * 1024, false);
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.enable().await;

    // Write 2 sectors (1024 bytes) starting at sector 2.
    let data: Vec<u8> = (0..1024).map(|i| ((i * 7 + 3) % 256) as u8).collect();
    harness.post_write_request(0, 2, &data);
    let (_used_id, _used_len) = harness.wait_for_used().await;

    // Read 2 sectors back from sector 2.
    let data_gpa = harness.post_read_request(3, 2, 1024);
    let (_used_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_len, 1025); // 1024 data + 1 status

    let mut readback = vec![0u8; 1024];
    harness.mem.read_at(data_gpa, &mut readback).unwrap();
    assert_eq!(readback, data);
}

/// Three sequential requests: write, read, flush — verifies the device
/// correctly processes a sequence of different operations.
#[async_test]
async fn sequential_write_read_flush(driver: DefaultDriver) {
    let disk = ram_disk(64 * 1024, false);
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.enable().await;

    // Write
    let pattern = [0xDE; 512];
    harness.post_write_request(0, 0, &pattern);
    let (used_id, _) = harness.wait_for_used().await;
    assert_eq!(used_id, 0);

    // Read
    let data_gpa = harness.post_read_request(3, 0, 512);
    let (used_id, _) = harness.wait_for_used().await;
    assert_eq!(used_id, 3);

    let mut buf = [0u8; 512];
    harness.mem.read_at(data_gpa, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0xDE));

    // Flush
    harness.post_flush_request(5);
    let (used_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_id, 5);
    assert_eq!(used_len, 1);
}

/// Verify that the sector conversion uses right-shift (not left-shift)
/// when the disk's native sector size exceeds 512 bytes.
///
/// This test uses a 512-byte sector disk (where sector_shift == 0, so
/// the shift direction doesn't matter), and then writes/reads at specific
/// sectors to verify the data lands at the correct offset.
///
/// The real proof that the fix is correct is a unit test below that
/// directly checks the arithmetic. The integration test ensures the
/// full request path works at non-zero sector offsets.
#[async_test]
async fn sector_offset_correctness(driver: DefaultDriver) {
    let disk = ram_disk(64 * 1024, false); // 128 × 512-byte sectors
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.enable().await;

    // Write to sector 10.
    let data = [0xAA; 512];
    harness.post_write_request(0, 10, &data);
    harness.wait_for_used().await;

    // Write different data to sector 11.
    let data2 = [0xBB; 512];
    harness.post_write_request(3, 11, &data2);
    harness.wait_for_used().await;

    // Read sector 10 — should be 0xAA.
    let gpa10 = harness.post_read_request(6, 10, 512);
    harness.wait_for_used().await;
    let mut buf = [0u8; 512];
    harness.mem.read_at(gpa10, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0xAA), "sector 10 data wrong");

    // Read sector 11 — should be 0xBB.
    let gpa11 = harness.post_read_request(8, 11, 512);
    harness.wait_for_used().await;
    harness.mem.read_at(gpa11, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0xBB), "sector 11 data wrong");

    // Read sector 9 — should be zeroes (never written).
    let gpa9 = harness.post_read_request(10, 9, 512);
    harness.wait_for_used().await;
    harness.mem.read_at(gpa9, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0), "sector 9 should be zeroes");
}

// --- 4K-sector test disk ---

/// A simple in-memory disk with configurable sector size, used to test the
/// sector shift conversion path with non-512-byte sectors.
#[derive(Inspect)]
struct TestDisk4K {
    sector_size: u32,
    #[inspect(skip)]
    storage: Mutex<Vec<u8>>,
    #[inspect(skip)]
    supports_discard: bool,
}

impl TestDisk4K {
    fn new(total_bytes: usize, sector_size: u32) -> Self {
        assert!(sector_size.is_power_of_two() && sector_size >= 512);
        assert_eq!(total_bytes % sector_size as usize, 0);
        Self {
            sector_size,
            storage: Mutex::new(vec![0u8; total_bytes]),
            supports_discard: false,
        }
    }

    fn with_discard(mut self) -> Self {
        self.supports_discard = true;
        self
    }
}

impl DiskIo for TestDisk4K {
    fn disk_type(&self) -> &str {
        "test-4k"
    }

    fn sector_count(&self) -> u64 {
        self.storage.lock().len() as u64 / self.sector_size as u64
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        None
    }

    fn physical_sector_size(&self) -> u32 {
        self.sector_size
    }

    fn is_fua_respected(&self) -> bool {
        false
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        let offset = sector as usize * self.sector_size as usize;
        let end = offset + buffers.len();
        let storage = self.storage.lock();
        if end > storage.len() {
            return Err(DiskError::IllegalBlock);
        }
        buffers.writer().write(&storage[offset..end])?;
        Ok(())
    }

    async fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        _fua: bool,
    ) -> Result<(), DiskError> {
        let offset = sector as usize * self.sector_size as usize;
        let end = offset + buffers.len();
        let mut storage = self.storage.lock();
        if end > storage.len() {
            return Err(DiskError::IllegalBlock);
        }
        buffers.reader().read(&mut storage[offset..end])?;
        Ok(())
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        Ok(())
    }

    async fn unmap(
        &self,
        _sector: u64,
        _count: u64,
        _block_level_only: bool,
    ) -> Result<(), DiskError> {
        Ok(())
    }

    fn unmap_behavior(&self) -> disk_backend::UnmapBehavior {
        if self.supports_discard {
            disk_backend::UnmapBehavior::Unspecified
        } else {
            disk_backend::UnmapBehavior::Ignored
        }
    }
}

// --- Sector shift regression tests ---

/// Write and read via a 4096-byte-sector disk to exercise the sector shift
/// conversion. The virtio protocol always uses 512-byte sector numbers, so
/// writing to virtio sector 8 means byte offset 4096, which is disk sector 1
/// on a 4K disk.
///
/// With the old bug (`<< sector_shift`), virtio sector 8 became disk sector
/// `8 << 3 = 64`, which is well beyond the disk — the IO would fail or
/// silently corrupt. With the fix (`>> sector_shift`), it correctly maps
/// to disk sector `8 >> 3 = 1`.
#[async_test]
async fn write_read_4k_sector_disk(driver: DefaultDriver) {
    // 64 KiB disk with 4096-byte sectors → 16 disk sectors.
    let disk = Disk::new(TestDisk4K::new(64 * 1024, 4096)).unwrap();
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.enable().await;

    // Write to virtio sector 8 (= byte offset 4096 = disk sector 1).
    let data = [0xAA; 4096];
    harness.post_write_request(0, 8, &data);
    let (_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_len, 1); // status byte only

    // Read it back from the same virtio sector.
    let data_gpa = harness.post_read_request(3, 8, 4096);
    let (_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_len, 4097); // 4096 data + 1 status

    let mut readback = vec![0u8; 4096];
    harness.mem.read_at(data_gpa, &mut readback).unwrap();
    assert!(
        readback.iter().all(|&b| b == 0xAA),
        "data mismatch: sector shift conversion is wrong"
    );

    // Verify the adjacent sectors are still zeroes (no misplaced writes).
    let gpa0 = harness.post_read_request(5, 0, 4096);
    harness.wait_for_used().await;
    let mut buf = vec![0u8; 4096];
    harness.mem.read_at(gpa0, &mut buf).unwrap();
    assert!(
        buf.iter().all(|&b| b == 0),
        "sector 0 should be zeroes (data written to wrong location)"
    );
}

/// Write at various 512-byte-granularity offsets on a 4K disk and verify
/// they land at the correct disk positions.
#[async_test]
async fn sector_shift_multiple_offsets_4k(driver: DefaultDriver) {
    // 128 KiB disk with 4096-byte sectors → 32 disk sectors.
    let disk = Disk::new(TestDisk4K::new(128 * 1024, 4096)).unwrap();
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.enable().await;

    // Write different patterns to virtio sectors 0, 16, and 24.
    // Virtio sector 0  → disk sector 0  (byte offset 0)
    // Virtio sector 16 → disk sector 2  (byte offset 8192)
    // Virtio sector 24 → disk sector 3  (byte offset 12288)
    let patterns: &[(u64, u8)] = &[(0, 0x11), (16, 0x22), (24, 0x33)];

    let mut desc = 0u16;
    for &(sector, pattern) in patterns {
        let data = vec![pattern; 4096];
        harness.post_write_request(desc, sector, &data);
        harness.wait_for_used().await;
        desc += 3; // each write uses 3 descriptors
    }

    // Read them all back.
    for &(sector, pattern) in patterns {
        let gpa = harness.post_read_request(desc, sector, 4096);
        harness.wait_for_used().await;
        let mut buf = vec![0u8; 4096];
        harness.mem.read_at(gpa, &mut buf).unwrap();
        assert!(
            buf.iter().all(|&b| b == pattern),
            "mismatch at virtio sector {sector}: expected 0x{pattern:02x}"
        );
        desc += 2; // each read uses 2 descriptors
    }

    // Virtio sector 8 (disk sector 1) was never written — should be zeroes.
    let gpa = harness.post_read_request(desc, 8, 4096);
    harness.wait_for_used().await;
    let mut buf = vec![0u8; 4096];
    harness.mem.read_at(gpa, &mut buf).unwrap();
    assert!(
        buf.iter().all(|&b| b == 0),
        "virtio sector 8 should be zeroes"
    );
}

// --- Discard integration tests ---

/// Submit a discard request on a fresh harness and assert the expected status.
async fn check_discard(
    driver: &DefaultDriver,
    disk: Disk,
    read_only: bool,
    sector: u64,
    num_sectors: u32,
    flags: u32,
    expected_status: u8,
) {
    let mut harness = TestHarness::new(driver, disk, read_only);
    harness.enable().await;
    let status_gpa = harness.post_discard_request(0, sector, num_sectors, flags);
    let (_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_len, 1);
    assert_eq!(harness.read_status(status_gpa), expected_status);
}

fn test_disk_4k_discard() -> Disk {
    Disk::new(TestDisk4K::new(64 * 1024, 4096).with_discard()).unwrap()
}

/// Discard with properly aligned sector and num_sectors on a 4K disk
/// should succeed.
#[async_test]
async fn discard_aligned_succeeds(driver: DefaultDriver) {
    // Discard virtio sector 8 (disk sector 1), num_sectors=8 (8×512 = 4096).
    check_discard(
        &driver,
        test_disk_4k_discard(),
        false,
        8,
        8,
        0,
        VIRTIO_BLK_S_OK,
    )
    .await;
}

/// Discard with num_sectors not aligned to the backend sector size (4K)
/// should fail with IOERR. This is the bug the alignment validation
/// fix was added to catch.
#[async_test]
async fn discard_misaligned_num_sectors_fails(driver: DefaultDriver) {
    // num_sectors=5 is not a multiple of 8 (4096/512).
    check_discard(
        &driver,
        test_disk_4k_discard(),
        false,
        0,
        5,
        0,
        VIRTIO_BLK_S_IOERR,
    )
    .await;
}

/// Discard with sector not aligned to the backend sector size (4K)
/// should fail with IOERR.
#[async_test]
async fn discard_misaligned_sector_fails(driver: DefaultDriver) {
    // sector=3 is not aligned to 8 (4096/512).
    check_discard(
        &driver,
        test_disk_4k_discard(),
        false,
        3,
        8,
        0,
        VIRTIO_BLK_S_IOERR,
    )
    .await;
}

/// Discard with the unmap flag set should be rejected with UNSUPP
/// per spec §5.2.6.2.
#[async_test]
async fn discard_with_unmap_flag_returns_unsupp(driver: DefaultDriver) {
    check_discard(
        &driver,
        test_disk_4k_discard(),
        false,
        0,
        8,
        1,
        VIRTIO_BLK_S_UNSUPP,
    )
    .await;
}

/// Discard on a read-only disk should fail with IOERR.
#[async_test]
async fn discard_on_read_only_disk_fails(driver: DefaultDriver) {
    check_discard(
        &driver,
        test_disk_4k_discard(),
        true,
        0,
        8,
        0,
        VIRTIO_BLK_S_IOERR,
    )
    .await;
}

/// Discard on a 512-byte-sector disk (no shift) should succeed even with
/// num_sectors values that would fail on a 4K disk — the alignment check
/// is sector-size-dependent.
#[async_test]
async fn discard_512b_sector_any_count_succeeds(driver: DefaultDriver) {
    // sector_shift=0, sector_mask=0 → any num_sectors is "aligned".
    check_discard(
        &driver,
        ram_disk(64 * 1024, false),
        false,
        0,
        5,
        0,
        VIRTIO_BLK_S_OK,
    )
    .await;
}

// --- Bounce buffer integration tests ---

/// Write and read using a descriptor chain that forces the bounce buffer
/// fallback, then verify the data survives the roundtrip.
///
/// The bounce buffer path is exercised when `try_build_gpn_list` fails,
/// which happens when the data payload is split across multiple descriptors
/// whose GPAs are non-contiguous and have non-page-aligned boundaries.
///
/// This test places two 256-byte data fragments at GPAs that are separated
/// by a gap and sit at non-page-aligned offsets, making PagedRange
/// construction impossible. The device must fall back to copying through
/// the bounce buffer for both write and read.
#[async_test]
async fn bounce_buffer_write_read_roundtrip(driver: DefaultDriver) {
    let disk = ram_disk(64 * 1024, false);
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.enable().await;

    let frag_size: u32 = 256;

    // --- Write: header + 2 non-contiguous data fragments + status ---

    // Place fragments at non-page-aligned GPAs separated by a gap.
    // alloc_data places them contiguously, so we manually allocate with a gap.
    let header_gpa = harness.alloc_data(REQ_HEADER_SIZE);
    let frag1_gpa = harness.alloc_data(frag_size);
    // Skip 100 bytes to create a non-page-aligned gap.
    let _gap = harness.alloc_data(100);
    let frag2_gpa = harness.alloc_data(frag_size);
    let status_gpa = harness.alloc_data(1);

    // Verify the fragments are at non-page-aligned, non-contiguous GPAs.
    assert_ne!(
        frag1_gpa + frag_size as u64,
        frag2_gpa,
        "fragments must not be contiguous"
    );
    assert_ne!(frag1_gpa % 4096, 0, "frag1 should not be page-aligned");

    // Write request header.
    let header = VirtioBlkReqHeader {
        request_type: VIRTIO_BLK_T_OUT,
        reserved: 0,
        sector: 0,
    };
    harness.mem.write_at(header_gpa, header.as_bytes()).unwrap();

    // Write recognizable patterns into the two fragments.
    let pattern1: Vec<u8> = (0..frag_size).map(|i| (i % 251) as u8).collect();
    let pattern2: Vec<u8> = (0..frag_size).map(|i| ((i + 100) % 251) as u8).collect();
    harness.mem.write_at(frag1_gpa, &pattern1).unwrap();
    harness.mem.write_at(frag2_gpa, &pattern2).unwrap();
    harness.mem.write_at(status_gpa, &[0xFFu8]).unwrap();

    // Build descriptor chain: header → frag1 → frag2 → status
    let d = 0u16;
    write_descriptor(
        &harness.mem,
        DESC_ADDR,
        d,
        header_gpa,
        REQ_HEADER_SIZE,
        DescriptorFlags::new().with_next(true),
        d + 1,
    );
    write_descriptor(
        &harness.mem,
        DESC_ADDR,
        d + 1,
        frag1_gpa,
        frag_size,
        DescriptorFlags::new().with_next(true),
        d + 2,
    );
    write_descriptor(
        &harness.mem,
        DESC_ADDR,
        d + 2,
        frag2_gpa,
        frag_size,
        DescriptorFlags::new().with_next(true),
        d + 3,
    );
    write_descriptor(
        &harness.mem,
        DESC_ADDR,
        d + 3,
        status_gpa,
        1,
        DescriptorFlags::new().with_write(true),
        0,
    );
    make_available(
        &harness.mem,
        AVAIL_ADDR,
        QUEUE_SIZE,
        d,
        &mut harness.avail_idx,
    );
    harness.queue_event.signal();

    let (_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_len, 1); // status byte only

    let mut status = [0u8; 1];
    harness.mem.read_at(status_gpa, &mut status).unwrap();
    assert_eq!(status[0], VIRTIO_BLK_S_OK, "write should succeed");

    // --- Read back using similarly fragmented descriptors ---

    let header_gpa2 = harness.alloc_data(REQ_HEADER_SIZE);
    let read_frag1_gpa = harness.alloc_data(frag_size);
    let _gap2 = harness.alloc_data(100);
    let read_frag2_gpa = harness.alloc_data(frag_size);
    let read_status_gpa = harness.alloc_data(1);

    let header2 = VirtioBlkReqHeader {
        request_type: VIRTIO_BLK_T_IN,
        reserved: 0,
        sector: 0,
    };
    harness
        .mem
        .write_at(header_gpa2, header2.as_bytes())
        .unwrap();

    // Descriptor chain: header (readable) → frag1 + frag2 + status (writable)
    let d = 4u16;
    write_descriptor(
        &harness.mem,
        DESC_ADDR,
        d,
        header_gpa2,
        REQ_HEADER_SIZE,
        DescriptorFlags::new().with_next(true),
        d + 1,
    );
    write_descriptor(
        &harness.mem,
        DESC_ADDR,
        d + 1,
        read_frag1_gpa,
        frag_size,
        DescriptorFlags::new().with_write(true).with_next(true),
        d + 2,
    );
    write_descriptor(
        &harness.mem,
        DESC_ADDR,
        d + 2,
        read_frag2_gpa,
        frag_size,
        DescriptorFlags::new().with_write(true).with_next(true),
        d + 3,
    );
    write_descriptor(
        &harness.mem,
        DESC_ADDR,
        d + 3,
        read_status_gpa,
        1,
        DescriptorFlags::new().with_write(true),
        0,
    );
    make_available(
        &harness.mem,
        AVAIL_ADDR,
        QUEUE_SIZE,
        d,
        &mut harness.avail_idx,
    );
    harness.queue_event.signal();

    let (_id, used_len) = harness.wait_for_used().await;
    // 512 bytes data + 1 status byte
    assert_eq!(used_len, frag_size * 2 + 1);

    // Verify the read-back data matches what we wrote.
    let mut readback1 = vec![0u8; frag_size as usize];
    let mut readback2 = vec![0u8; frag_size as usize];
    harness.mem.read_at(read_frag1_gpa, &mut readback1).unwrap();
    harness.mem.read_at(read_frag2_gpa, &mut readback2).unwrap();
    assert_eq!(readback1, pattern1, "bounce buffer read frag1 mismatch");
    assert_eq!(readback2, pattern2, "bounce buffer read frag2 mismatch");

    let mut read_status = [0u8; 1];
    harness
        .mem
        .read_at(read_status_gpa, &mut read_status)
        .unwrap();
    assert_eq!(read_status[0], VIRTIO_BLK_S_OK, "read should succeed");
}
