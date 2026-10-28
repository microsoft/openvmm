// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the virtio-blk device.
//!
//! These tests construct a full `VirtioBlkDevice` with guest memory, a RAM
//! disk backend, and real virtio queues — then drive requests through the
//! descriptor ring just as a guest driver would.

use crate::VirtioBlkDevice;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::UnmapBehavior;
use disk_hook::Disk;
use disk_hook::DiskOperation;
use disk_hook::HookDisk;
use futures::future::Either;
use futures::future::select;
use guestmem::GuestMemory;
use inspect::Inspect;
use pal_async::DefaultDriver;
use pal_async::DefaultPool;
use pal_async::async_test;
use pal_async::timer::PolledTimer;
use pal_event::Event;
use parking_lot::Mutex;
use scsi_buffers::RequestBuffers;
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use test_with_tracing::test;
use virtio::QueueResources;
use virtio::VirtioDevice;
use virtio::queue::QueueParams;
use virtio::queue::QueueState;
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
        Self::with_device_driver(driver, driver, disk, read_only, None)
    }

    /// Like [`TestHarness::new`], but runs the device's worker task on
    /// `device_driver` rather than on the test's own executor. A test that
    /// must stay responsive while the worker misbehaves puts the device on a
    /// separate thread's executor.
    fn with_device_driver(
        driver: &DefaultDriver,
        device_driver: &DefaultDriver,
        disk: Disk,
        read_only: bool,
        serial: Option<String>,
    ) -> Self {
        let mem = GuestMemory::allocate(TOTAL_MEM_SIZE);

        init_avail_ring(&mem, AVAIL_ADDR);
        init_used_ring(&mem, USED_ADDR);

        let driver_source =
            VmTaskDriverSource::new(SingleDriverBackend::new(device_driver.clone()));
        let device = VirtioBlkDevice::new(&driver_source, disk, read_only, serial).unwrap();

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
        self.start_with_state(None).await;
    }

    async fn start_with_state(&mut self, state: Option<QueueState>) {
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
                state,
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
    fn post_write_request(&mut self, head_desc: u16, sector: u64, data: &[u8]) -> u64 {
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
        data_gpa
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
    HookDisk::new(size, 512)
        .with_read_only(read_only)
        .into_disk()
}

/// Awaits `fut`, panicking with `msg` if it does not complete within `timeout`.
async fn with_timeout<F: Future>(
    driver: &DefaultDriver,
    timeout: Duration,
    msg: &str,
    fut: F,
) -> F::Output {
    let mut timer = PolledTimer::new(driver);
    match select(pin!(fut), pin!(timer.sleep(timeout))).await {
        Either::Left((output, _)) => output,
        Either::Right(_) => panic!("{msg}"),
    }
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

#[async_test]
async fn get_id_returns_disk_id(driver: DefaultDriver) {
    let disk = HookDisk::new(64 * 1024, 512)
        .with_disk_id(*b"backing-disk-id!")
        .with_fua_respected(false)
        .with_unmap_behavior(UnmapBehavior::Ignored)
        .into_disk();
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.enable().await;

    let id_gpa = harness.post_get_id_request(0);
    let (used_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_id, 0);
    assert_eq!(used_len, VIRTIO_BLK_ID_BYTES as u32 + 1);

    let mut id_buf = [0u8; VIRTIO_BLK_ID_BYTES];
    harness.mem.read_at(id_gpa, &mut id_buf).unwrap();
    assert_eq!(&id_buf, b"6261636b696e672d6469");
}

#[async_test]
async fn get_id_returns_configured_serial(driver: DefaultDriver) {
    let disk = HookDisk::new(64 * 1024, 512)
        .with_disk_id(*b"backing-disk-id!")
        .with_fua_respected(false)
        .with_unmap_behavior(UnmapBehavior::Ignored)
        .into_disk();
    let mut serial = [0; VIRTIO_BLK_ID_BYTES];
    serial[..13].copy_from_slice(b"custom-serial");
    let mut harness = TestHarness::with_device_driver(
        &driver,
        &driver,
        disk,
        false,
        Some("custom-serial".into()),
    );
    harness.enable().await;

    let id_gpa = harness.post_get_id_request(0);
    let (used_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_id, 0);
    assert_eq!(used_len, VIRTIO_BLK_ID_BYTES as u32 + 1);

    let mut id_buf = [0u8; VIRTIO_BLK_ID_BYTES];
    harness.mem.read_at(id_gpa, &mut id_buf).unwrap();
    assert_eq!(id_buf, serial);
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

// Hooks do not receive buffers, so this decorator only checks buffer identity
// and policy. HookDisk provides storage, metadata, operation counts, and gates.
#[derive(Inspect)]
struct BufferProbeDisk {
    disk: Disk,
    #[inspect(skip)]
    probe: Arc<IoProbe>,
}

struct IoProbe {
    copy_only: bool,
    guest_base: AtomicUsize,
    read_ops: AtomicUsize,
    write_ops: AtomicUsize,
    entered: Event,
    gate: Mutex<Option<mesh::OneshotReceiver<()>>>,
}

impl Default for IoProbe {
    fn default() -> Self {
        Self {
            copy_only: false,
            guest_base: AtomicUsize::new(0),
            read_ops: AtomicUsize::new(0),
            write_ops: AtomicUsize::new(0),
            entered: Event::new(),
            gate: Mutex::new(None),
        }
    }
}

impl IoProbe {
    fn disk(self: &Arc<Self>, size: u64) -> Disk {
        let probe = self.clone();
        let disk = HookDisk::new(size, 512)
            .with_fua_respected(false)
            .with_unmap_behavior(UnmapBehavior::Ignored)
            .with_async_hook(move |operation| {
                let probe = probe.clone();
                async move {
                    match operation {
                        DiskOperation::Read { .. } => {
                            probe.read_ops.fetch_add(1, Ordering::SeqCst);
                        }
                        DiskOperation::Write { .. } => {
                            probe.write_ops.fetch_add(1, Ordering::SeqCst);
                        }
                        _ => return Ok(()),
                    }
                    probe.entered.signal();
                    let gate = probe.gate.lock().take();
                    if let Some(gate) = gate {
                        gate.await.unwrap();
                    }
                    Ok(())
                }
            })
            .into_disk();
        Disk::new(BufferProbeDisk {
            disk,
            probe: self.clone(),
        })
        .unwrap()
    }

    async fn wait_entered(&self, driver: &DefaultDriver) {
        let mut entered = pal_async::wait::PolledWait::new(driver, self.entered.clone()).unwrap();
        let mut timer = PolledTimer::new(driver);
        match select(
            pin!(entered.wait()),
            pin!(timer.sleep(Duration::from_secs(5))),
        )
        .await
        {
            Either::Left((result, _)) => result.unwrap(),
            Either::Right(_) => panic!("timed out waiting for backend submission"),
        }
    }

    fn check(&self, buffers: &RequestBuffers<'_>, is_read: bool) {
        let memory = buffers.guest_memory();
        if self.copy_only {
            assert_eq!(memory.io_policy(), guestmem::GuestMemoryIoPolicy::CopyOnly);
            assert!(!memory.supports_kernel_lock());
            assert!(memory.full_mapping().is_none());
            assert!(buffers.lock(is_read).is_err());
        } else {
            assert_eq!(memory.io_policy(), guestmem::GuestMemoryIoPolicy::Direct);
            assert!(memory.supports_kernel_lock());
            let (base, _) = memory.full_mapping().unwrap();
            assert_ne!(base as usize, self.guest_base.load(Ordering::SeqCst));
        }
    }
}

impl DiskIo for BufferProbeDisk {
    fn disk_type(&self) -> &str {
        "buffer-probe"
    }

    fn sector_count(&self) -> u64 {
        self.disk.sector_count()
    }

    fn sector_size(&self) -> u32 {
        self.disk.sector_size()
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        self.disk.disk_id()
    }

    fn physical_sector_size(&self) -> u32 {
        self.disk.physical_sector_size()
    }

    fn is_fua_respected(&self) -> bool {
        self.disk.is_fua_respected()
    }

    fn is_read_only(&self) -> bool {
        self.disk.is_read_only()
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        self.probe.check(buffers, true);
        self.disk.read_vectored(buffers, sector).await
    }

    async fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        self.probe.check(buffers, false);
        self.disk.write_vectored(buffers, sector, fua).await
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        self.disk.sync_cache().await
    }

    async fn unmap(
        &self,
        sector: u64,
        count: u64,
        block_level_only: bool,
    ) -> Result<(), DiskError> {
        self.disk.unmap(sector, count, block_level_only).await
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        self.disk.unmap_behavior()
    }
}

#[async_test]
async fn copy_only_aligned_io_reaches_backend_without_staging(driver: DefaultDriver) {
    let probe = Arc::new(IoProbe {
        copy_only: true,
        ..Default::default()
    });
    let disk = probe.disk(64 * 1024);
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.mem = harness.mem.copy_only_view();
    assert!(harness.device.supports_save_restore());
    assert_eq!(harness.device.config.size_max, 0);
    assert_eq!(harness.device.config.seg_max, crate::DEFAULT_SEG_MAX);
    assert_eq!(
        harness.device.traits().device_features.bank(0) & VIRTIO_BLK_F_SIZE_MAX,
        0
    );
    harness.enable().await;
    harness.next_data_offset = DATA_BASE + guestmem::PAGE_SIZE as u64 - REQ_HEADER_SIZE as u64;
    let pattern = vec![0x5a; guestmem::PAGE_SIZE];
    let data_gpa = harness.post_write_request(0, 0, &pattern);
    assert!(data_gpa.is_multiple_of(guestmem::PAGE_SIZE as u64));
    assert_eq!(harness.wait_for_used().await.1, 1);
    harness.next_data_offset = DATA_BASE + 3 * guestmem::PAGE_SIZE as u64 - REQ_HEADER_SIZE as u64;
    let read_gpa = harness.post_read_request(3, 0, pattern.len() as u32);
    assert!(read_gpa.is_multiple_of(guestmem::PAGE_SIZE as u64));
    assert_eq!(harness.wait_for_used().await.1, pattern.len() as u32 + 1);
    let mut actual = vec![0; pattern.len()];
    harness.mem.read_at(read_gpa, &mut actual).unwrap();
    assert_eq!(actual, pattern);
    assert_eq!(probe.write_ops.load(Ordering::SeqCst), 1);
    assert_eq!(probe.read_ops.load(Ordering::SeqCst), 1);
    harness.device.stop_queue(0).await;
}

#[async_test]
async fn copy_only_read_waits_for_backend_before_completion(driver: DefaultDriver) {
    let probe = Arc::new(IoProbe {
        copy_only: true,
        ..Default::default()
    });
    let (release, gate) = mesh::oneshot();
    *probe.gate.lock() = Some(gate);
    let disk = probe.disk(64 * 1024);
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.mem = harness.mem.copy_only_view();
    harness.enable().await;
    let data_gpa = harness.post_read_request(0, 0, 512);
    harness.mem.write_at(data_gpa, &[0x9d; 512]).unwrap();
    probe.wait_entered(&driver).await;
    let mut actual = [0; 512];
    harness.mem.read_at(data_gpa, &mut actual).unwrap();
    assert_eq!(actual, [0x9d; 512]);
    assert_eq!(
        virtio::test_helpers::read_used_idx(&harness.mem, USED_ADDR),
        0
    );
    release.send(());
    assert_eq!(harness.wait_for_used().await.1, 513);
    harness.mem.read_at(data_gpa, &mut actual).unwrap();
    assert_eq!(actual, [0; 512]);
    harness.device.stop_queue(0).await;
}

#[async_test]
async fn copy_only_ram_backend_copies_payload(driver: DefaultDriver) {
    let disk = ram_disk(64 * 1024, false);
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.mem = harness.mem.copy_only_view();
    harness.enable().await;
    let pattern = [0x67; 4096];
    let write_gpa = harness.post_write_request(0, 8, &pattern);
    assert_eq!(harness.wait_for_used().await.1, 1);
    assert_eq!(
        harness.read_status(write_gpa + pattern.len() as u64),
        VIRTIO_BLK_S_OK
    );
    let read_gpa = harness.post_read_request(3, 8, pattern.len() as u32);
    assert_eq!(harness.wait_for_used().await.1, pattern.len() as u32 + 1);
    assert_eq!(
        harness.read_status(read_gpa + pattern.len() as u64),
        VIRTIO_BLK_S_OK
    );
    let mut actual = [0; 4096];
    harness.mem.read_at(read_gpa, &mut actual).unwrap();
    assert_eq!(actual, pattern);
    harness.device.stop_queue(0).await;
}

#[async_test]
async fn copy_only_aligned_request_above_one_mib_reaches_backend(driver: DefaultDriver) {
    const DATA_SIZE: u32 = 2 * 1024 * 1024;
    const DATA_ADDR: u64 = 0x20000;
    let probe = Arc::new(IoProbe {
        copy_only: true,
        ..Default::default()
    });
    let disk = probe.disk(4 * 1024 * 1024);
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.mem = GuestMemory::allocate(4 * 1024 * 1024).copy_only_view();
    init_avail_ring(&harness.mem, AVAIL_ADDR);
    init_used_ring(&harness.mem, USED_ADDR);
    harness.enable().await;
    let header = VirtioBlkReqHeader {
        request_type: VIRTIO_BLK_T_IN,
        reserved: 0,
        sector: 0,
    };
    harness.mem.write_at(DATA_BASE, header.as_bytes()).unwrap();
    write_descriptor(
        &harness.mem,
        DESC_ADDR,
        0,
        DATA_BASE,
        REQ_HEADER_SIZE,
        DescriptorFlags::new().with_next(true),
        1,
    );
    write_descriptor(
        &harness.mem,
        DESC_ADDR,
        1,
        DATA_ADDR,
        DATA_SIZE + 1,
        DescriptorFlags::new().with_write(true),
        0,
    );
    harness
        .mem
        .fill_at(DATA_ADDR, 0x55, DATA_SIZE as usize)
        .unwrap();
    make_available(
        &harness.mem,
        AVAIL_ADDR,
        QUEUE_SIZE,
        0,
        &mut harness.avail_idx,
    );
    harness.queue_event.signal();
    assert_eq!(harness.wait_for_used().await.1, DATA_SIZE + 1);
    assert_eq!(
        harness.read_status(DATA_ADDR + u64::from(DATA_SIZE)),
        VIRTIO_BLK_S_OK
    );
    let mut actual = vec![0xff; DATA_SIZE as usize];
    harness.mem.read_at(DATA_ADDR, &mut actual).unwrap();
    assert!(actual.iter().all(|&byte| byte == 0));
    assert_eq!(probe.read_ops.load(Ordering::SeqCst), 1);
    harness.device.stop_queue(0).await;
}

#[async_test]
async fn backend_error_completes_and_allows_further_io(driver: DefaultDriver) {
    for copy_only in [false, true] {
        let probe = Arc::new(IoProbe {
            copy_only,
            ..Default::default()
        });
        let disk = probe.disk(64 * 1024);
        let mut harness = TestHarness::new(&driver, disk, false);
        if copy_only {
            harness.mem = harness.mem.copy_only_view();
        }
        harness.enable().await;
        let data = harness.post_read_request(0, 128, 512);
        assert_eq!(harness.wait_for_used().await.1, 1);
        assert_eq!(harness.read_status(data + 512), VIRTIO_BLK_S_IOERR);
        assert_eq!(probe.read_ops.load(Ordering::SeqCst), 1);
        let data = harness.post_read_request(3, 0, 512);
        assert_eq!(harness.wait_for_used().await.1, 513);
        assert_eq!(harness.read_status(data + 512), VIRTIO_BLK_S_OK);
        harness.device.stop_queue(0).await;
    }
}

#[async_test]
async fn stop_drains_inflight_io_after_status_write_failure(driver: DefaultDriver) {
    for copy_only in [false, true] {
        let probe = Arc::new(IoProbe {
            copy_only,
            ..Default::default()
        });
        let (release, gate) = mesh::oneshot();
        *probe.gate.lock() = Some(gate);
        let disk = probe.disk(64 * 1024);
        let mut harness = TestHarness::new(&driver, disk, false);
        if copy_only {
            harness.mem = harness.mem.copy_only_view();
        }
        harness.enable().await;
        harness.post_read_request(0, 0, 512);
        write_descriptor(
            &harness.mem,
            DESC_ADDR,
            1,
            TOTAL_MEM_SIZE as u64 - 512,
            513,
            DescriptorFlags::new().with_write(true),
            0,
        );
        probe.wait_entered(&driver).await;
        {
            let mut stop = pin!(harness.device.stop_queue(0));
            assert!(futures::poll!(&mut stop).is_pending());
            release.send(());
            assert_eq!(stop.await.unwrap().used_index, 1);
        }
        let (worker, queue) = harness.device.worker.get_mut();
        assert!(queue.is_none());
        assert!(worker.ios.is_empty());
        assert_eq!(probe.read_ops.load(Ordering::SeqCst), 1);
        assert_eq!(
            virtio::test_helpers::read_used_idx(&harness.mem, USED_ADDR),
            1
        );
    }
}

#[async_test]
async fn io_survives_pause_resume(driver: DefaultDriver) {
    for copy_only in [false, true] {
        let probe = Arc::new(IoProbe {
            copy_only,
            ..Default::default()
        });
        let disk = probe.disk(64 * 1024);
        let mut harness = TestHarness::new(&driver, disk, false);
        if copy_only {
            harness.mem = harness.mem.copy_only_view();
        }
        harness.enable().await;
        assert!(harness.device.supports_save_restore());
        let pattern = [0x5a; 512];
        harness.post_write_request(0, 7, &pattern);
        assert_eq!(harness.wait_for_used().await.1, 1);
        let state = harness.device.stop_queue(0).await.unwrap();
        assert_eq!(
            state,
            QueueState {
                avail_index: 1,
                used_index: 1,
            }
        );
        assert!(harness.device.stop_queue(0).await.is_none());
        let data_gpa = harness.post_read_request(3, 7, 512);
        harness.start_with_state(Some(state)).await;
        assert_eq!(harness.wait_for_used().await.1, 513);
        let mut data = [0; 512];
        harness.mem.read_at(data_gpa, &mut data).unwrap();
        assert_eq!(data, pattern);
        assert!(harness.device.supports_save_restore());
        let state = harness.device.stop_queue(0).await.unwrap();
        assert_eq!(
            state,
            QueueState {
                avail_index: 2,
                used_index: 2,
            }
        );
        assert_eq!(probe.write_ops.load(Ordering::SeqCst), 1);
        assert_eq!(probe.read_ops.load(Ordering::SeqCst), 1);
    }
}

#[async_test]
async fn large_scattered_request_still_succeeds(driver: DefaultDriver) {
    let probe = Arc::new(IoProbe::default());
    let disk = probe.disk(4 * 1024 * 1024);
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.mem = GuestMemory::allocate(4 * 1024 * 1024);
    probe.guest_base.store(
        harness.mem.full_mapping().unwrap().0 as usize,
        Ordering::SeqCst,
    );
    init_avail_ring(&harness.mem, AVAIL_ADDR);
    init_used_ring(&harness.mem, USED_ADDR);
    harness.enable().await;
    let header = VirtioBlkReqHeader {
        request_type: VIRTIO_BLK_T_OUT,
        reserved: 0,
        sector: 0,
    };
    harness.mem.write_at(DATA_BASE, header.as_bytes()).unwrap();
    write_descriptor(
        &harness.mem,
        DESC_ADDR,
        0,
        DATA_BASE,
        REQ_HEADER_SIZE,
        DescriptorFlags::new().with_next(true),
        1,
    );
    for (index, address, next) in [(1, 0x11200, 2), (2, 0x201200, 3)] {
        harness.mem.fill_at(address, 0x65, 1024 * 1024).unwrap();
        write_descriptor(
            &harness.mem,
            DESC_ADDR,
            index,
            address,
            1024 * 1024,
            DescriptorFlags::new().with_next(true),
            next,
        );
    }
    write_descriptor(
        &harness.mem,
        DESC_ADDR,
        3,
        0x11000,
        1,
        DescriptorFlags::new().with_write(true),
        0,
    );
    make_available(
        &harness.mem,
        AVAIL_ADDR,
        QUEUE_SIZE,
        0,
        &mut harness.avail_idx,
    );
    harness.queue_event.signal();
    assert_eq!(harness.wait_for_used().await.1, 1);
    let mut status = [0xff];
    harness.mem.read_at(0x11000, &mut status).unwrap();
    assert_eq!(status[0], VIRTIO_BLK_S_OK);
    assert_eq!(probe.write_ops.load(Ordering::SeqCst), 1);
    harness.device.stop_queue(0).await;
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
    let disk = HookDisk::new(64 * 1024, 4096)
        .with_fua_respected(false)
        .with_unmap_behavior(UnmapBehavior::Ignored)
        .into_disk();
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
    let disk = HookDisk::new(128 * 1024, 4096)
        .with_fua_respected(false)
        .with_unmap_behavior(UnmapBehavior::Ignored)
        .into_disk();
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
    HookDisk::new(64 * 1024, 4096)
        .with_fua_respected(false)
        .with_unmap_behavior(UnmapBehavior::Unspecified)
        .into_disk()
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

#[async_test]
async fn bounce_buffer_write_read_roundtrip(driver: DefaultDriver) {
    scattered_write_read_roundtrip(driver, false).await;
}

#[async_test]
async fn copy_only_scattered_io_uses_gathering_fallback(driver: DefaultDriver) {
    scattered_write_read_roundtrip(driver, true).await;
}

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
async fn scattered_write_read_roundtrip(driver: DefaultDriver, copy_only: bool) {
    let disk = ram_disk(64 * 1024, false);
    let mut harness = TestHarness::new(&driver, disk, false);
    if copy_only {
        harness.mem = harness.mem.copy_only_view();
    }
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

/// A descriptor whose `next` link points back at itself forms a chain that
/// never terminates, and the queue rejects it without consuming it — the
/// available index does not move, so the same chain is still there on the next
/// poll. A worker that logs the error and keeps polling therefore spins inside
/// a single `poll` call, never returning `Pending`. That burns a CPU forever
/// and, because the cancel future is only polled once the work future returns
/// `Pending`, leaves the worker task impossible to stop — wedging device
/// teardown, reset, save/restore, and inspect.
///
/// The device runs on its own executor thread so that a spinning worker wedges
/// only that thread, letting this test fail on a timeout instead of hanging.
#[async_test]
async fn cyclic_descriptor_chain_does_not_wedge_worker(driver: DefaultDriver) {
    let (_device_thread, device_driver) = DefaultPool::spawn_on_thread("virtio-blk-device");
    let disk = ram_disk(64 * 1024, false);
    let mut harness = TestHarness::with_device_driver(&driver, &device_driver, disk, false, None);
    harness.enable().await;

    // Run one valid request first, so the worker is known to be up and parked
    // waiting for a kick by the time the bad chain arrives.
    harness.post_flush_request(0);
    let (used_id, _) = harness.wait_for_used().await;
    assert_eq!(used_id, 0);

    // Descriptor 4 chains to itself.
    let gpa = harness.alloc_data(REQ_HEADER_SIZE);
    write_descriptor(
        &harness.mem,
        DESC_ADDR,
        4,
        gpa,
        REQ_HEADER_SIZE,
        DescriptorFlags::new().with_next(true),
        4,
    );
    make_available(
        &harness.mem,
        AVAIL_ADDR,
        QUEUE_SIZE,
        4,
        &mut harness.avail_idx,
    );
    harness.queue_event.signal();

    // Give the worker time to wake on the kick and reject the chain.
    PolledTimer::new(&driver)
        .sleep(Duration::from_millis(250))
        .await;

    // A worker spinning on the bad chain never observes the stop request.
    with_timeout(
        &driver,
        Duration::from_secs(5),
        "virtio-blk worker could not be stopped after an invalid descriptor chain",
        harness.device.stop_queue(0),
    )
    .await;
}
