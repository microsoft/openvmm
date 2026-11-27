// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the virtio-blk device.
//!
//! These tests construct a full `VirtioBlkDevice` with guest memory and real
//! virtio queues, then drive requests through the descriptor ring using RAM
//! or instrumented backends.

use crate::VirtioBlkDevice;
use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use disk_backend::UnmapBehavior;
use disk_hook::Disk;
use disk_hook::DiskError;
use disk_hook::DiskOperation;
use disk_hook::HookDisk;
use futures::future::Either;
use futures::future::select;
use guestmem::GuestMemory;
use pal_async::DefaultDriver;
use pal_async::DefaultPool;
use pal_async::async_test;
use pal_async::timer::PolledTimer;
use pal_event::Event;
use parking_lot::Mutex;
use pci_core::msi::MsiConnection;
use std::future::Future;
use std::future::poll_fn;
use std::pin::pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU16;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;
use test_with_tracing::test;
use virtio::PciInterruptModel;
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
use virtio::test_helpers::read_used_idx;
use virtio::test_helpers::wait_for_used;
use virtio::test_helpers::write_descriptor;
use virtio::transport::VirtioPciDevice;
use virtio_resources::blk::VirtioBlkOptions;
use vmcore::device_state::ChangeDeviceState;
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
        let resources = self.resources();
        self.device
            .start_queue(0, resources, &VirtioDeviceFeatures::new(), None)
            .await
            .unwrap();
    }

    fn resources(&self) -> QueueResources {
        QueueResources {
            params: QueueParams {
                size: QUEUE_SIZE,
                enable: true,
                desc_addr: DESC_ADDR,
                avail_addr: AVAIL_ADDR,
                used_addr: USED_ADDR,
            },
            notify: Interrupt::from_event(self.interrupt_event.clone()),
            event: self.queue_event.clone(),
            guest_memory: self.mem.clone(),
        }
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
    /// Returns the data buffer GPA; the status byte follows the data.
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

        let sentinel = vec![0xFFu8; (data_len + 1) as usize];
        self.mem.write_at(data_gpa, &sentinel).unwrap();

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
    /// Returns the GPA of the status byte.
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
        status_gpa
    }

    /// Build a flush request descriptor chain and return the status byte GPA.
    fn post_flush_request(&mut self, head_desc: u16) -> u64 {
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
        status_gpa
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

#[async_test]
async fn multiqueue_completes_independently_and_restarts(driver: DefaultDriver) {
    let (started, started_recv) = async_channel::unbounded();
    let (release, release_recv) = async_channel::bounded(1);
    let disk = HookDisk::new(64 * 1024, 512)
        .with_async_hook(move |op| {
            let started = started.clone();
            let release = release_recv.clone();
            async move {
                let write = matches!(op, DiskOperation::Write { .. });
                started.send(write).await.unwrap();
                if write {
                    release.recv().await.unwrap();
                }
                Ok(())
            }
        })
        .into_disk();
    let source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
    let mut first = TestHarness::new(&driver, disk.clone(), false);
    first.device = VirtioBlkDevice::with_options(
        &source,
        disk.clone(),
        false,
        None,
        VirtioBlkOptions {
            queue_count: 2,
            io_depth: 1,
        },
    )
    .unwrap();
    let mut second = TestHarness::new(&driver, disk, false);
    let features = VirtioDeviceFeatures::new().with_device_specific_low(VIRTIO_BLK_F_MQ);
    let resources = first.resources();
    first
        .device
        .start_queue(0, resources, &features, None)
        .await
        .unwrap();
    first
        .device
        .start_queue(1, second.resources(), &features, None)
        .await
        .unwrap();

    let status = first.post_write_request(0, 0, &[0xa5; 512]);
    let write = with_timeout(
        &driver,
        Duration::from_secs(5),
        "first queue did not start its write",
        started_recv.recv(),
    )
    .await
    .unwrap();
    assert!(write);
    second.post_read_request(0, 1, 512);
    assert_eq!(second.wait_for_used().await, (0, 513));
    let write = started_recv.recv().await.unwrap();
    assert!(!write);
    assert_eq!(read_used_idx(&first.mem, USED_ADDR), 0);

    let saved = first.device.stop_queue(1).await.unwrap();
    release.send(()).await.unwrap();
    first.device.stop_queue(0).await.unwrap();
    assert_eq!(first.wait_for_used().await, (0, 1));
    assert_eq!(first.read_status(status), VIRTIO_BLK_S_OK);

    first
        .device
        .start_queue(1, second.resources(), &features, Some(saved))
        .await
        .unwrap();
    let data = second.post_read_request(2, 0, 512);
    assert_eq!(second.wait_for_used().await, (2, 513));
    let mut actual = [0; 512];
    second.mem.read_at(data, &mut actual).unwrap();
    assert_eq!(actual, [0xa5; 512]);
    first.device.stop_queue(1).await.unwrap();
}

#[async_test]
async fn multiqueue_requires_negotiation_and_valid_queue_indices(driver: DefaultDriver) {
    let source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
    let disk = ram_disk(64 * 1024, false);
    let mut harness = TestHarness::new(&driver, disk.clone(), false);
    harness.device = VirtioBlkDevice::with_options(
        &source,
        disk.clone(),
        false,
        None,
        VirtioBlkOptions {
            queue_count: 2,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(harness.device.traits().max_queues, 2);
    assert_ne!(
        harness
            .device
            .traits()
            .device_features
            .device_specific_low()
            & VIRTIO_BLK_F_MQ,
        0
    );
    let resources = harness.resources();
    assert!(
        harness
            .device
            .start_queue(1, resources, &VirtioDeviceFeatures::new(), None)
            .await
            .is_err()
    );
    let resources = harness.resources();
    assert!(
        harness
            .device
            .start_queue(2, resources, &harness.device.traits().device_features, None)
            .await
            .is_err()
    );
    assert!(harness.device.stop_queue(2).await.is_none());
    harness.enable().await;
    harness.post_read_request(0, 0, 512);
    assert_eq!(harness.wait_for_used().await, (0, 513));
    harness.device.stop_queue(0).await.unwrap();

    for (queue_count, io_depth) in [(0, 64), (257, 64), (1, 0)] {
        assert!(
            VirtioBlkDevice::with_options(
                &source,
                disk.clone(),
                false,
                None,
                VirtioBlkOptions {
                    queue_count,
                    io_depth,
                },
            )
            .is_err()
        );
    }
}

#[async_test]
async fn configured_depth_backpressures_and_resumes_queue(driver: DefaultDriver) {
    let (started, started_recv) = async_channel::unbounded();
    let (release, release_recv) = async_channel::unbounded();
    let disk = HookDisk::new(64 * 1024, 512)
        .with_async_hook(move |_| {
            let started = started.clone();
            let release = release_recv.clone();
            async move {
                started.send(()).await.unwrap();
                release.recv().await.unwrap();
                Ok(())
            }
        })
        .into_disk();
    let source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
    let mut harness = TestHarness::new(&driver, disk.clone(), false);
    harness.device = VirtioBlkDevice::with_options(
        &source,
        disk,
        false,
        None,
        VirtioBlkOptions {
            io_depth: 2,
            ..Default::default()
        },
    )
    .unwrap();
    harness.enable().await;
    for head in [0, 2, 4] {
        harness.post_read_request(head, 0, 512);
    }
    for _ in 0..2 {
        with_timeout(
            &driver,
            Duration::from_secs(5),
            "queue did not fill configured depth",
            started_recv.recv(),
        )
        .await
        .unwrap();
    }
    PolledTimer::new(&driver)
        .sleep(Duration::from_millis(20))
        .await;
    assert!(started_recv.try_recv().is_err());
    assert_eq!(read_used_idx(&harness.mem, USED_ADDR), 0);
    release.send(()).await.unwrap();
    with_timeout(
        &driver,
        Duration::from_secs(5),
        "queue did not resume after an I/O completed",
        started_recv.recv(),
    )
    .await
    .unwrap();
    release.send(()).await.unwrap();
    release.send(()).await.unwrap();
    let mut completed = Vec::new();
    for _ in 0..3 {
        let (head, len) = harness.wait_for_used().await;
        assert_eq!(len, 513);
        completed.push(head);
    }
    completed.sort();
    assert_eq!(completed, [0, 2, 4]);
    harness.device.stop_queue(0).await.unwrap();
}

fn ram_disk(size: u64, read_only: bool) -> Disk {
    HookDisk::new(size, 512)
        .with_read_only(read_only)
        .into_disk()
}

struct RequeueResources {
    mem: GuestMemory,
    queue_event: Event,
    avail_idx: AtomicU16,
}

impl RequeueResources {
    fn make_ready(&self) {
        let mut avail_idx = self.avail_idx.fetch_add(1, Ordering::SeqCst);
        make_available(&self.mem, AVAIL_ADDR, QUEUE_SIZE, 0, &mut avail_idx);
        self.queue_event.signal();
    }
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

#[async_test]
async fn teardown_releases_disk_backend(driver: DefaultDriver) {
    // The disk owns its hook, so this weak reference observes when the last
    // disk handle is dropped.
    let backend = Arc::new(());
    let backend_released = Arc::downgrade(&backend);
    let disk = HookDisk::new(64 * 1024, 512)
        .with_sync_hook(move |_| {
            let _backend = &backend;
            Ok(())
        })
        .into_disk();
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
    let device = VirtioBlkDevice::new(&driver_source, disk, false, None).unwrap();
    let msi = MsiConnection::new();
    let mut transport = VirtioPciDevice::new(
        Box::new(device),
        &driver,
        GuestMemory::empty(),
        PciInterruptModel::Msix(&msi.target()),
        None,
        &mut ExternallyManagedMmioIntercepts,
        None,
    )
    .unwrap();

    transport.start();
    transport.stop().await;
    assert!(
        backend_released.upgrade().is_some(),
        "stop must retain the disk backend for resume"
    );

    transport.teardown().await;
    // Do not yield: removal must release the disk before returning.
    assert!(
        backend_released.upgrade().is_none(),
        "teardown returned before the disk backend was released"
    );
}

#[async_test]
async fn stop_drains_one_accepted_write_exactly_once(driver: DefaultDriver) {
    let (_device_thread, device_driver) = DefaultPool::spawn_on_thread("virtio-blk-drain");
    let write_calls = Arc::new(AtomicUsize::new(0));
    let (write_started, write_started_recv) = std::sync::mpsc::channel();
    let (release_write, release_write_recv) = async_channel::bounded(1);
    let disk = HookDisk::new(64 * 1024, 512)
        .with_async_hook({
            let write_calls = write_calls.clone();
            move |op| {
                let hold = matches!(op, DiskOperation::Write { .. })
                    && write_calls.fetch_add(1, Ordering::SeqCst) == 0;
                if hold {
                    write_started.send(()).unwrap();
                }
                let release_write = release_write_recv.clone();
                async move {
                    if hold {
                        release_write.recv().await.unwrap();
                    }
                    Ok(())
                }
            }
        })
        .into_disk();
    let mut harness = TestHarness::with_device_driver(&driver, &device_driver, disk, false, None);
    harness.enable().await;
    let status_gpa = harness.post_write_request(0, 0, &[0xa5; 512]);
    write_started_recv
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    let stopped = {
        let mut stop = pin!(harness.device.stop_queue(0));
        assert!(
            stop.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        release_write.send(()).await.unwrap();
        with_timeout(
            &driver,
            Duration::from_secs(5),
            "stop failed to drain accepted I/O",
            stop,
        )
        .await
    };
    assert!(stopped.is_some());
    assert_eq!(harness.wait_for_used().await, (0, 1));
    assert_eq!(harness.read_status(status_gpa), VIRTIO_BLK_S_OK);
    assert_eq!(write_calls.load(Ordering::SeqCst), 1);
    assert_eq!(read_used_idx(&harness.mem, USED_ADDR), 1);
}

#[async_test]
async fn stop_preempts_continuously_ready_queue(driver: DefaultDriver) {
    let (_device_thread, device_driver) = DefaultPool::spawn_on_thread("virtio-blk-ready");
    let resources = Arc::new(OnceLock::<RequeueResources>::new());
    let keep_requeueing = Arc::new(AtomicBool::new(true));
    let (write_started, write_started_recv) = std::sync::mpsc::channel();
    let disk = HookDisk::new(64 * 1024, 512)
        .with_sync_hook({
            let resources = resources.clone();
            let keep_requeueing = keep_requeueing.clone();
            let write_calls = AtomicUsize::new(0);
            move |op| {
                if matches!(op, DiskOperation::Write { .. }) {
                    if write_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        write_started.send(()).unwrap();
                    }
                    // Keep the loop ready before the current request finishes.
                    if keep_requeueing.load(Ordering::SeqCst) {
                        resources.get().unwrap().make_ready();
                    }
                }
                Ok(())
            }
        })
        .into_disk();
    let mut harness = TestHarness::with_device_driver(&driver, &device_driver, disk, false, None);
    assert!(
        resources
            .set(RequeueResources {
                mem: harness.mem.clone(),
                queue_event: harness.queue_event.clone(),
                avail_idx: AtomicU16::new(1),
            })
            .is_ok()
    );
    harness.enable().await;
    harness.post_write_request(0, 0, &[0xa5; 512]);
    write_started_recv
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    let mut stop = pin!(harness.device.stop_queue(0));
    let mut timer = PolledTimer::new(&driver);
    let mut timeout = pin!(timer.sleep(Duration::from_secs(5)));
    let stopped = poll_fn(|cx| {
        if let Poll::Ready(stopped) = stop.as_mut().poll(cx) {
            Poll::Ready(Some(stopped))
        } else if timeout.as_mut().poll(cx).is_ready() {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    })
    .await;
    keep_requeueing.store(false, Ordering::SeqCst);
    let Some(stopped) = stopped else {
        with_timeout(
            &driver,
            Duration::from_secs(5),
            "stop failed after requeueing ended",
            stop,
        )
        .await;
        panic!("stop was starved by continuously ready work");
    };
    assert!(stopped.is_some());
}

/// Write 1 sector then read it back. Verifies basic write and read roundtrip.
#[async_test]
async fn write_then_read_roundtrip(driver: DefaultDriver) {
    let disk = ram_disk(64 * 1024, false);
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.enable().await;

    // Write a recognizable pattern to sector 0.
    let data: Vec<u8> = (0..512).map(|i| (i % 251) as u8).collect();
    let status_gpa = harness.post_write_request(0, 0, &data);
    let (used_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_id, 0);
    // used_len = 1 (status byte only for writes)
    assert_eq!(used_len, 1);
    assert_eq!(harness.read_status(status_gpa), VIRTIO_BLK_S_OK);

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
    assert_eq!(harness.read_status(data_gpa + 512), VIRTIO_BLK_S_OK);

    let mut readback = vec![0xFFu8; 512];
    harness.mem.read_at(data_gpa, &mut readback).unwrap();
    assert!(readback.iter().all(|&b| b == 0), "expected all zeroes");
}

/// Write to a read-only disk — should fail with IOERR status.
#[async_test]
async fn write_to_read_only_disk_fails(driver: DefaultDriver) {
    let (backend, state) = instrumented_disk(64 * 1024, 512);
    let mut harness = TestHarness::new(&driver, backend.into_disk(), true);
    harness.enable().await;

    let status_gpa = harness.post_write_request(0, 0, &[0xAB; 512]);
    assert_eq!(harness.wait_for_used().await, (0, 1));
    assert_eq!(harness.read_status(status_gpa), VIRTIO_BLK_S_IOERR);
    assert!(state.lock().calls.is_empty());
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
    let (backend, state) = instrumented_disk(64 * 1024, 512);
    let mut harness = TestHarness::new(&driver, backend.into_disk(), false);
    harness.enable().await;

    let status_gpa = harness.post_raw_request(0, 0xFF, 0);
    let (used_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_id, 0);
    assert_eq!(used_len, 1);

    let status = harness.read_status(status_gpa);
    assert_eq!(status, VIRTIO_BLK_S_UNSUPP);
    assert!(state.lock().calls.is_empty());
}

/// Write to multiple sectors then read them back to verify multi-sector IO.
#[async_test]
async fn multi_sector_write_read(driver: DefaultDriver) {
    let disk = ram_disk(64 * 1024, false);
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.enable().await;

    // Write 2 sectors (1024 bytes) starting at sector 2.
    let data: Vec<u8> = (0..1024).map(|i| ((i * 7 + 3) % 256) as u8).collect();
    let status_gpa = harness.post_write_request(0, 2, &data);
    assert_eq!(harness.wait_for_used().await, (0, 1));
    assert_eq!(harness.read_status(status_gpa), VIRTIO_BLK_S_OK);

    // Read 2 sectors back from sector 2.
    let data_gpa = harness.post_read_request(3, 2, 1024);
    let (_used_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_len, 1025); // 1024 data + 1 status
    assert_eq!(harness.read_status(data_gpa + 1024), VIRTIO_BLK_S_OK);

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

    let pattern = [0xDE; 512];
    let status_gpa = harness.post_write_request(0, 0, &pattern);
    assert_eq!(harness.wait_for_used().await, (0, 1));
    assert_eq!(harness.read_status(status_gpa), VIRTIO_BLK_S_OK);

    let data_gpa = harness.post_read_request(3, 0, 512);
    assert_eq!(harness.wait_for_used().await, (3, 513));
    assert_eq!(harness.read_status(data_gpa + 512), VIRTIO_BLK_S_OK);
    let mut buf = [0u8; 512];
    harness.mem.read_at(data_gpa, &mut buf).unwrap();
    assert_eq!(buf, pattern);

    let status_gpa = harness.post_flush_request(5);
    assert_eq!(harness.wait_for_used().await, (5, 1));
    assert_eq!(harness.read_status(status_gpa), VIRTIO_BLK_S_OK);
}

/// Sector addresses are unchanged for a backend with 512-byte sectors.
#[async_test]
async fn sector_offset_correctness(driver: DefaultDriver) {
    let (backend, state) = instrumented_disk(64 * 1024, 512);
    let mut harness = TestHarness::new(&driver, backend.into_disk(), false);
    harness.enable().await;

    // Write to sector 10.
    let data = [0xAA; 512];
    let status_gpa = harness.post_write_request(0, 10, &data);
    assert_eq!(harness.wait_for_used().await, (0, 1));
    assert_eq!(harness.read_status(status_gpa), VIRTIO_BLK_S_OK);

    // Write different data to sector 11.
    let data2 = [0xBB; 512];
    let status_gpa = harness.post_write_request(3, 11, &data2);
    assert_eq!(harness.wait_for_used().await, (3, 1));
    assert_eq!(harness.read_status(status_gpa), VIRTIO_BLK_S_OK);

    // Read sector 10 — should be 0xAA.
    let gpa10 = harness.post_read_request(6, 10, 512);
    assert_eq!(harness.wait_for_used().await, (6, 513));
    assert_eq!(harness.read_status(gpa10 + 512), VIRTIO_BLK_S_OK);
    let mut buf = [0u8; 512];
    harness.mem.read_at(gpa10, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0xAA), "sector 10 data wrong");

    // Read sector 11 — should be 0xBB.
    let gpa11 = harness.post_read_request(8, 11, 512);
    assert_eq!(harness.wait_for_used().await, (8, 513));
    assert_eq!(harness.read_status(gpa11 + 512), VIRTIO_BLK_S_OK);
    harness.mem.read_at(gpa11, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0xBB), "sector 11 data wrong");

    // Read sector 9 — should be zeroes (never written).
    let gpa9 = harness.post_read_request(10, 9, 512);
    assert_eq!(harness.wait_for_used().await, (10, 513));
    assert_eq!(harness.read_status(gpa9 + 512), VIRTIO_BLK_S_OK);
    harness.mem.read_at(gpa9, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0), "sector 9 should be zeroes");
    assert_eq!(
        state.lock().calls,
        [
            DiskOperation::Write {
                sector: 10,
                len: 512,
                fua: false
            },
            DiskOperation::Write {
                sector: 11,
                len: 512,
                fua: false
            },
            DiskOperation::Read {
                sector: 10,
                len: 512
            },
            DiskOperation::Read {
                sector: 11,
                len: 512
            },
            DiskOperation::Read {
                sector: 9,
                len: 512
            },
        ]
    );
}

#[derive(Default)]
struct TestDiskState {
    calls: Vec<DiskOperation>,
    next_error: Option<DiskError>,
}

fn instrumented_disk(size: u64, sector_size: u32) -> (HookDisk, Arc<Mutex<TestDiskState>>) {
    let state = Arc::<Mutex<TestDiskState>>::default();
    let disk = HookDisk::new(size, sector_size)
        .with_fua_respected(false)
        .with_unmap_behavior(UnmapBehavior::Ignored)
        .with_sync_hook({
            let state = Arc::clone(&state);
            move |operation| {
                let mut state = state.lock();
                state.calls.push(operation);
                match state.next_error.take() {
                    Some(error) => Err(error),
                    None => Ok(()),
                }
            }
        });
    (disk, state)
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
    let status_gpa = harness.post_write_request(0, 8, &data);
    assert_eq!(harness.wait_for_used().await, (0, 1));
    assert_eq!(harness.read_status(status_gpa), VIRTIO_BLK_S_OK);

    // Read it back from the same virtio sector.
    let data_gpa = harness.post_read_request(3, 8, 4096);
    assert_eq!(harness.wait_for_used().await, (3, 4097));
    assert_eq!(harness.read_status(data_gpa + 4096), VIRTIO_BLK_S_OK);

    let mut readback = vec![0u8; 4096];
    harness.mem.read_at(data_gpa, &mut readback).unwrap();
    assert!(
        readback.iter().all(|&b| b == 0xAA),
        "data mismatch: sector shift conversion is wrong"
    );

    // Verify the adjacent sectors are still zeroes (no misplaced writes).
    let gpa0 = harness.post_read_request(5, 0, 4096);
    assert_eq!(harness.wait_for_used().await, (5, 4097));
    assert_eq!(harness.read_status(gpa0 + 4096), VIRTIO_BLK_S_OK);
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
    let (backend, state) = instrumented_disk(128 * 1024, 4096);
    let mut harness = TestHarness::new(&driver, backend.into_disk(), false);
    harness.enable().await;

    // Write different patterns to virtio sectors 0, 16, and 24.
    // Virtio sector 0  → disk sector 0  (byte offset 0)
    // Virtio sector 16 → disk sector 2  (byte offset 8192)
    // Virtio sector 24 → disk sector 3  (byte offset 12288)
    let patterns: &[(u64, u8)] = &[(0, 0x11), (16, 0x22), (24, 0x33)];

    let mut desc = 0u16;
    for &(sector, pattern) in patterns {
        let data = vec![pattern; 4096];
        let status_gpa = harness.post_write_request(desc, sector, &data);
        assert_eq!(harness.wait_for_used().await, (desc, 1));
        assert_eq!(harness.read_status(status_gpa), VIRTIO_BLK_S_OK);
        desc += 3; // each write uses 3 descriptors
    }

    // Read them all back.
    for &(sector, pattern) in patterns {
        let gpa = harness.post_read_request(desc, sector, 4096);
        assert_eq!(harness.wait_for_used().await, (desc, 4097));
        assert_eq!(harness.read_status(gpa + 4096), VIRTIO_BLK_S_OK);
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
    assert_eq!(harness.wait_for_used().await, (desc, 4097));
    assert_eq!(harness.read_status(gpa + 4096), VIRTIO_BLK_S_OK);
    let mut buf = vec![0u8; 4096];
    harness.mem.read_at(gpa, &mut buf).unwrap();
    assert!(
        buf.iter().all(|&b| b == 0),
        "virtio sector 8 should be zeroes"
    );
    assert_eq!(
        state.lock().calls,
        [
            DiskOperation::Write {
                sector: 0,
                len: 4096,
                fua: false
            },
            DiskOperation::Write {
                sector: 2,
                len: 4096,
                fua: false
            },
            DiskOperation::Write {
                sector: 3,
                len: 4096,
                fua: false
            },
            DiskOperation::Read {
                sector: 0,
                len: 4096
            },
            DiskOperation::Read {
                sector: 2,
                len: 4096
            },
            DiskOperation::Read {
                sector: 3,
                len: 4096
            },
            DiskOperation::Read {
                sector: 1,
                len: 4096
            },
        ]
    );
}

// --- Discard integration tests ---

/// Submit a discard request, check its completion, and return the backend calls.
async fn check_discard(
    driver: &DefaultDriver,
    sector_size: u32,
    read_only: bool,
    sector: u64,
    num_sectors: u32,
    flags: u32,
    expected_status: u8,
) -> Vec<DiskOperation> {
    let (backend, state) = instrumented_disk(64 * 1024, sector_size);
    let disk = backend
        .with_unmap_behavior(UnmapBehavior::Unspecified)
        .into_disk();
    let mut harness = TestHarness::new(driver, disk, read_only);
    harness.enable().await;
    let status_gpa = harness.post_discard_request(0, sector, num_sectors, flags);
    assert_eq!(harness.wait_for_used().await, (0, 1));
    assert_eq!(harness.read_status(status_gpa), expected_status);
    std::mem::take(&mut state.lock().calls)
}

/// Discard with properly aligned sector and num_sectors on a 4K disk
/// should succeed.
#[async_test]
async fn discard_aligned_succeeds(driver: DefaultDriver) {
    let calls = check_discard(&driver, 4096, false, 8, 16, 0, VIRTIO_BLK_S_OK).await;
    assert_eq!(
        calls,
        [DiskOperation::Unmap {
            sector: 1,
            count: 2,
            block_level_only: false
        }]
    );
}

/// Discard with num_sectors not aligned to the backend sector size (4K)
/// should fail with IOERR. This is the bug the alignment validation
/// fix was added to catch.
#[async_test]
async fn discard_misaligned_num_sectors_fails(driver: DefaultDriver) {
    // num_sectors=5 is not a multiple of 8 (4096/512).
    let calls = check_discard(&driver, 4096, false, 0, 5, 0, VIRTIO_BLK_S_IOERR).await;
    assert!(calls.is_empty());
}

/// Discard with sector not aligned to the backend sector size (4K)
/// should fail with IOERR.
#[async_test]
async fn discard_misaligned_sector_fails(driver: DefaultDriver) {
    // sector=3 is not aligned to 8 (4096/512).
    let calls = check_discard(&driver, 4096, false, 3, 8, 0, VIRTIO_BLK_S_IOERR).await;
    assert!(calls.is_empty());
}

/// Discard with the unmap flag set should be rejected with UNSUPP
/// per spec §5.2.6.2.
#[async_test]
async fn discard_with_unmap_flag_returns_unsupp(driver: DefaultDriver) {
    let calls = check_discard(&driver, 4096, false, 0, 8, 1, VIRTIO_BLK_S_UNSUPP).await;
    assert!(calls.is_empty());
}

/// Discard on a read-only disk should fail with IOERR.
#[async_test]
async fn discard_on_read_only_disk_fails(driver: DefaultDriver) {
    let calls = check_discard(&driver, 4096, true, 0, 8, 0, VIRTIO_BLK_S_IOERR).await;
    assert!(calls.is_empty());
}

/// Discard on a 512-byte-sector disk (no shift) should succeed even with
/// num_sectors values that would fail on a 4K disk — the alignment check
/// is sector-size-dependent.
#[async_test]
async fn discard_512b_sector_any_count_succeeds(driver: DefaultDriver) {
    // sector_shift=0, sector_mask=0 → any num_sectors is "aligned".
    let calls = check_discard(&driver, 512, false, 3, 5, 0, VIRTIO_BLK_S_OK).await;
    assert_eq!(
        calls,
        [DiskOperation::Unmap {
            sector: 3,
            count: 5,
            block_level_only: false
        }]
    );
}

#[async_test]
async fn misaligned_read_write_never_reach_backend(driver: DefaultDriver) {
    for is_read in [true, false] {
        for (sector, len) in [(3, 4096), (8, 512)] {
            let (backend, state) = instrumented_disk(64 * 1024, 4096);
            let mut harness = TestHarness::new(&driver, backend.into_disk(), false);
            harness.enable().await;

            let status_gpa = if is_read {
                harness.post_read_request(0, sector, len) + u64::from(len)
            } else {
                harness.post_write_request(0, sector, &vec![0xAB; len as usize])
            };
            assert_eq!(harness.wait_for_used().await, (0, 1));
            assert_eq!(harness.read_status(status_gpa), VIRTIO_BLK_S_IOERR);
            assert!(state.lock().calls.is_empty());
        }
    }
}

#[async_test]
async fn flush_succeeds(driver: DefaultDriver) {
    let (backend, state) = instrumented_disk(64 * 1024, 4096);
    let mut harness = TestHarness::new(&driver, backend.into_disk(), false);
    harness.enable().await;

    let status_gpa = harness.post_flush_request(0);

    assert_eq!(harness.wait_for_used().await, (0, 1));
    assert_eq!(harness.read_status(status_gpa), VIRTIO_BLK_S_OK);
    assert_eq!(state.lock().calls, [DiskOperation::SyncCache]);
}

#[async_test]
async fn backend_errors_translate_to_ioerr(driver: DefaultDriver) {
    for request_type in [
        VIRTIO_BLK_T_IN,
        VIRTIO_BLK_T_OUT,
        VIRTIO_BLK_T_FLUSH,
        VIRTIO_BLK_T_DISCARD,
    ] {
        for error in [
            DiskError::IllegalBlock,
            DiskError::InvalidInput,
            DiskError::ReadOnly,
            DiskError::Io(std::io::Error::other("injected backend failure")),
        ] {
            let (backend, state) = instrumented_disk(64 * 1024, 4096);
            state.lock().next_error = Some(error);
            let disk = backend
                .with_unmap_behavior(UnmapBehavior::Unspecified)
                .into_disk();
            let mut harness = TestHarness::new(&driver, disk, false);
            harness.enable().await;

            let (status_gpa, expected_call) = match request_type {
                VIRTIO_BLK_T_IN => (
                    harness.post_read_request(0, 8, 8192) + 8192,
                    DiskOperation::Read {
                        sector: 1,
                        len: 8192,
                    },
                ),
                VIRTIO_BLK_T_OUT => (
                    harness.post_write_request(0, 8, &[0xAB; 8192]),
                    DiskOperation::Write {
                        sector: 1,
                        len: 8192,
                        fua: false,
                    },
                ),
                VIRTIO_BLK_T_FLUSH => (harness.post_flush_request(0), DiskOperation::SyncCache),
                VIRTIO_BLK_T_DISCARD => (
                    harness.post_discard_request(0, 8, 16, 0),
                    DiskOperation::Unmap {
                        sector: 1,
                        count: 2,
                        block_level_only: false,
                    },
                ),
                _ => unreachable!(),
            };
            assert_eq!(harness.wait_for_used().await, (0, 1));
            assert_eq!(
                harness.read_status(status_gpa),
                VIRTIO_BLK_S_IOERR,
                "request type {request_type} must propagate the backend failure"
            );
            let state = state.lock();
            assert_eq!(state.calls, [expected_call]);
            assert!(state.next_error.is_none());
        }
    }
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

/// Backend range errors must become IOERR without corrupting in-range data.
#[async_test]
async fn out_of_range_request_fails(driver: DefaultDriver) {
    let (backend, state) = instrumented_disk(64 * 1024, 4096);
    let mut harness = TestHarness::new(&driver, backend.into_disk(), false);
    harness.enable().await;

    let pattern = [0x5Au8; 4096];
    let status_gpa = harness.post_write_request(0, 0, &pattern);
    assert_eq!(harness.wait_for_used().await, (0, 1));
    assert_eq!(harness.read_status(status_gpa), VIRTIO_BLK_S_OK);

    // Aligned virtio sector 128 maps to backend sector 16, just past capacity.
    let data_gpa = harness.post_read_request(3, 128, 4096);
    assert_eq!(harness.wait_for_used().await, (3, 1));
    assert_eq!(
        harness.read_status(data_gpa + 4096),
        VIRTIO_BLK_S_IOERR,
        "out-of-range read should fail with IOERR"
    );

    let status_gpa = harness.post_write_request(5, 128, &[0xEE; 4096]);
    assert_eq!(harness.wait_for_used().await, (5, 1));
    assert_eq!(
        harness.read_status(status_gpa),
        VIRTIO_BLK_S_IOERR,
        "out-of-range write should fail with IOERR"
    );

    let check_gpa = harness.post_read_request(8, 0, 4096);
    assert_eq!(harness.wait_for_used().await, (8, 4097));
    assert_eq!(harness.read_status(check_gpa + 4096), VIRTIO_BLK_S_OK);
    let mut buf = [0u8; 4096];
    harness.mem.read_at(check_gpa, &mut buf).unwrap();
    assert_eq!(buf, pattern, "in-range data corrupted by out-of-range op");
    assert_eq!(
        state.lock().calls,
        [
            DiskOperation::Write {
                sector: 0,
                len: 4096,
                fua: false,
            },
            DiskOperation::Read {
                sector: 16,
                len: 4096,
            },
            DiskOperation::Write {
                sector: 16,
                len: 4096,
                fua: false,
            },
            DiskOperation::Read {
                sector: 0,
                len: 4096,
            },
        ]
    );
}
