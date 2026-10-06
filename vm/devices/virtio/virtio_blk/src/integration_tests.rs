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
use disk_layered::DiskLayer;
use disk_layered::LayerConfiguration;
use disk_layered::LayeredDisk;
use disklayer_vhdx::VhdxLayer;
use disklayer_vhdx::io::BlockingFile;
use futures::future::Either;
use futures::future::select;
use guestmem::GuestMemory;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
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
use std::time::Duration;
use test_with_tracing::test;
use vhdx::VhdxFile;
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

/// Parameters for building a Rust-VHDX-backed [`Disk`] fixture.
struct VhdxParams {
    /// Virtual disk size in bytes.
    disk_size: u64,
    /// Block size in bytes (multiple of 1 MiB). 0 selects the 2 MiB default.
    block_size: u32,
    /// Logical sector size (512 or 4096).
    logical_sector_size: u32,
    /// Physical sector size (512 or 4096).
    physical_sector_size: u32,
    /// If true, create a fixed (fully allocated) image; otherwise dynamic.
    fixed: bool,
    /// If true, open the image read-only.
    read_only: bool,
}

/// Build a [`Disk`] backed by the pure-Rust VHDX engine
/// (`virtqueue -> virtio-blk -> DiskIo -> Rust VHDX`).
///
/// Returns the [`tempfile::TempDir`] alongside the disk; the caller MUST keep
/// it alive for the duration of the test, otherwise the backing file is
/// deleted out from under the open disk.
async fn vhdx_disk(driver: &DefaultDriver, params: VhdxParams) -> (Disk, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.vhdx");

    // Create an empty VHDX with the requested geometry.
    let bf = BlockingFile::open(&path, false).unwrap();
    let mut create_params = vhdx::CreateParams {
        disk_size: params.disk_size,
        block_size: params.block_size,
        logical_sector_size: params.logical_sector_size,
        physical_sector_size: params.physical_sector_size,
        is_fully_allocated: params.fixed,
        ..Default::default()
    };
    vhdx::create(&bf, &mut create_params).await.unwrap();

    // Re-open and wrap as a VhdxLayer. `bf2` shares the same backing file so
    // data I/O on resolved ranges targets the same descriptor.
    let bf = BlockingFile::open(&path, params.read_only).unwrap();
    let bf2 = bf.clone();
    let vhdx = if params.read_only {
        VhdxFile::open(bf).read_only().await.unwrap()
    } else {
        VhdxFile::open(bf).writable(driver).await.unwrap()
    };
    let layer = VhdxLayer::new(vhdx, bf2, params.read_only);

    let layered = LayeredDisk::new(
        params.read_only,
        vec![LayerConfiguration {
            layer: DiskLayer::new(layer),
            write_through: false,
            read_cache: false,
        }],
    )
    .await
    .unwrap();

    (Disk::new(layered).unwrap(), dir)
}

/// Create an empty VHDX file of `size` bytes at `path` (512-byte sectors).
async fn create_vhdx_file(path: &std::path::Path, size: u64) {
    let bf = BlockingFile::open(path, false).unwrap();
    let mut params = vhdx::CreateParams {
        disk_size: size,
        ..Default::default()
    };
    vhdx::create(&bf, &mut params).await.unwrap();
}

/// Open an existing VHDX file at `path` as a [`Disk`].
///
/// When `read_only` is true the file is opened read-only (replaying a dirty log
/// if `allow_replay` is set); otherwise it is opened writable. The underlying
/// file handle is always opened writable so log replay I/O can proceed.
async fn open_vhdx_file_disk(
    driver: &DefaultDriver,
    path: &std::path::Path,
    read_only: bool,
    allow_replay: bool,
) -> Disk {
    let bf = BlockingFile::open(path, false).unwrap();
    let bf2 = bf.clone();
    let vhdx = if read_only {
        VhdxFile::open(bf)
            .allow_replay(allow_replay)
            .read_only()
            .await
            .unwrap()
    } else {
        VhdxFile::open(bf).writable(driver).await.unwrap()
    };
    let layer = VhdxLayer::new(vhdx, bf2, read_only);
    let layered = LayeredDisk::new(
        read_only,
        vec![LayerConfiguration {
            layer: DiskLayer::new(layer),
            write_through: false,
            read_cache: false,
        }],
    )
    .await
    .unwrap();
    Disk::new(layered).unwrap()
}

/// Storage backend under test. Parameterized tests run the same virtio-blk
/// request coverage against both a RAM disk and the pure-Rust VHDX engine.
#[derive(Clone, Copy, Debug)]
enum Backend {
    Ram,
    Vhdx,
}

/// A [`Disk`] plus any resources (e.g. the VHDX temp dir) that must outlive it.
struct BackendDisk {
    disk: Disk,
    _guard: Option<tempfile::TempDir>,
}

impl Backend {
    /// Build a non-differencing disk of `size` bytes with the given geometry.
    ///
    /// `logical` and `physical` must be equal for the RAM backend, which models
    /// a single sector size; the VHDX backend supports differing values (512e).
    async fn make(
        self,
        driver: &DefaultDriver,
        size: u64,
        logical: u32,
        physical: u32,
        read_only: bool,
    ) -> BackendDisk {
        match self {
            Backend::Ram => {
                assert_eq!(logical, physical, "RAM backend cannot model 512e");
                let disk = if logical == 512 {
                    disklayer_ram::ram_disk_with_sector_size(size, read_only, logical).unwrap()
                } else {
                    // Minimal, obviously-correct 4K backend (also supports discard).
                    Disk::new(TestDisk4K::new(size as usize, logical).with_discard()).unwrap()
                };
                BackendDisk { disk, _guard: None }
            }
            Backend::Vhdx => {
                let (disk, dir) = vhdx_disk(
                    driver,
                    VhdxParams {
                        disk_size: size,
                        block_size: 0,
                        logical_sector_size: logical,
                        physical_sector_size: physical,
                        fixed: false,
                        read_only,
                    },
                )
                .await;
                BackendDisk {
                    disk,
                    _guard: Some(dir),
                }
            }
        }
    }
}

/// Generate `ram_<name>` and `vhdx_<name>` `#[async_test]` wrappers that run
/// `<name>_impl` against each backend.
macro_rules! backend_variants {
    ($impl_fn:ident, $ram:ident, $vhdx:ident) => {
        #[async_test]
        async fn $ram(driver: DefaultDriver) {
            $impl_fn(&driver, Backend::Ram).await;
        }

        #[async_test]
        async fn $vhdx(driver: DefaultDriver) {
            $impl_fn(&driver, Backend::Vhdx).await;
        }
    };
}

backend_variants!(
    write_then_read_roundtrip_impl,
    ram_write_then_read_roundtrip,
    vhdx_write_then_read_roundtrip
);
backend_variants!(
    read_unwritten_sector_returns_zeroes_impl,
    ram_read_unwritten_sector_returns_zeroes,
    vhdx_read_unwritten_sector_returns_zeroes
);
backend_variants!(
    write_to_read_only_disk_fails_impl,
    ram_write_to_read_only_disk_fails,
    vhdx_write_to_read_only_disk_fails
);
backend_variants!(flush_succeeds_impl, ram_flush_succeeds, vhdx_flush_succeeds);
backend_variants!(
    multi_sector_write_read_impl,
    ram_multi_sector_write_read,
    vhdx_multi_sector_write_read
);
backend_variants!(
    sequential_write_read_flush_impl,
    ram_sequential_write_read_flush,
    vhdx_sequential_write_read_flush
);
backend_variants!(
    sector_offset_correctness_impl,
    ram_sector_offset_correctness,
    vhdx_sector_offset_correctness
);
backend_variants!(
    write_read_4k_sector_disk_impl,
    ram_write_read_4k_sector_disk,
    vhdx_write_read_4k_sector_disk
);
backend_variants!(
    sector_shift_multiple_offsets_4k_impl,
    ram_sector_shift_multiple_offsets_4k,
    vhdx_sector_shift_multiple_offsets_4k
);
backend_variants!(
    discard_aligned_succeeds_impl,
    ram_discard_aligned_succeeds,
    vhdx_discard_aligned_succeeds
);
backend_variants!(
    discard_misaligned_num_sectors_fails_impl,
    ram_discard_misaligned_num_sectors_fails,
    vhdx_discard_misaligned_num_sectors_fails
);
backend_variants!(
    discard_misaligned_sector_fails_impl,
    ram_discard_misaligned_sector_fails,
    vhdx_discard_misaligned_sector_fails
);
backend_variants!(
    discard_with_unmap_flag_returns_unsupp_impl,
    ram_discard_with_unmap_flag_returns_unsupp,
    vhdx_discard_with_unmap_flag_returns_unsupp
);
backend_variants!(
    discard_on_read_only_disk_fails_impl,
    ram_discard_on_read_only_disk_fails,
    vhdx_discard_on_read_only_disk_fails
);
backend_variants!(
    discard_512b_sector_any_count_succeeds_impl,
    ram_discard_512b_sector_any_count_succeeds,
    vhdx_discard_512b_sector_any_count_succeeds
);

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
async fn write_then_read_roundtrip_impl(driver: &DefaultDriver, backend: Backend) {
    let BackendDisk { disk, _guard } = backend.make(driver, 64 * 1024, 512, 512, false).await;
    let mut harness = TestHarness::new(driver, disk, false);
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
async fn read_unwritten_sector_returns_zeroes_impl(driver: &DefaultDriver, backend: Backend) {
    let BackendDisk { disk, _guard } = backend.make(driver, 64 * 1024, 512, 512, false).await;
    let mut harness = TestHarness::new(driver, disk, false);
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
async fn write_to_read_only_disk_fails_impl(driver: &DefaultDriver, backend: Backend) {
    let BackendDisk { disk, _guard } = backend.make(driver, 64 * 1024, 512, 512, true).await;
    let mut harness = TestHarness::new(driver, disk, true);
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
async fn flush_succeeds_impl(driver: &DefaultDriver, backend: Backend) {
    let BackendDisk { disk, _guard } = backend.make(driver, 64 * 1024, 512, 512, false).await;
    let mut harness = TestHarness::new(driver, disk, false);
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
    let disk =
        Disk::new(TestDisk4K::new(64 * 1024, 512).with_disk_id(*b"backing-disk-id!")).unwrap();
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
    let disk =
        Disk::new(TestDisk4K::new(64 * 1024, 512).with_disk_id(*b"backing-disk-id!")).unwrap();
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

/// GET_ID on a VHDX image must surface the image's SCSI VPD page 0x83
/// identifier (its Page 83 data GUID) as lowercase hex, rather than the generic
/// default string returned when the backend has no disk id.
#[async_test]
async fn vhdx_get_id_returns_page83_identifier(driver: DefaultDriver) {
    let BackendDisk { disk, _guard } = Backend::Vhdx
        .make(&driver, 64 * 1024, 512, 512, false)
        .await;

    // The VHDX backend exposes its Page 83 data as the disk id. Compute the
    // expected GET_ID response exactly as the device does: lowercase hex of the
    // 16 id bytes, truncated to the virtio id length.
    let disk_id = disk.disk_id().expect("vhdx exposes a page 83 identifier");
    let hex: String = disk_id.iter().map(|b| format!("{b:02x}")).collect();
    let mut expected = [0u8; VIRTIO_BLK_ID_BYTES];
    let copy_len = hex.len().min(VIRTIO_BLK_ID_BYTES);
    expected[..copy_len].copy_from_slice(&hex.as_bytes()[..copy_len]);

    let mut harness = TestHarness::new(&driver, disk, false);
    harness.enable().await;

    let id_gpa = harness.post_get_id_request(0);
    let (_used_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_len, VIRTIO_BLK_ID_BYTES as u32 + 1);

    let mut id_buf = [0u8; VIRTIO_BLK_ID_BYTES];
    harness.mem.read_at(id_gpa, &mut id_buf).unwrap();
    assert_eq!(
        id_buf, expected,
        "GET_ID should return the page 83 id as hex"
    );
    assert_ne!(
        &id_buf, b"openvmm-virtio-blk\0\0",
        "VHDX must not fall back to the default identifier"
    );
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
async fn multi_sector_write_read_impl(driver: &DefaultDriver, backend: Backend) {
    let BackendDisk { disk, _guard } = backend.make(driver, 64 * 1024, 512, 512, false).await;
    let mut harness = TestHarness::new(driver, disk, false);
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
async fn sequential_write_read_flush_impl(driver: &DefaultDriver, backend: Backend) {
    let BackendDisk { disk, _guard } = backend.make(driver, 64 * 1024, 512, 512, false).await;
    let mut harness = TestHarness::new(driver, disk, false);
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
async fn sector_offset_correctness_impl(driver: &DefaultDriver, backend: Backend) {
    let BackendDisk { disk, _guard } = backend.make(driver, 64 * 1024, 512, 512, false).await; // 128 × 512-byte sectors
    let mut harness = TestHarness::new(driver, disk, false);
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
    disk_id: Option<[u8; 16]>,
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
            disk_id: None,
            storage: Mutex::new(vec![0u8; total_bytes]),
            supports_discard: false,
        }
    }

    fn with_disk_id(mut self, disk_id: [u8; 16]) -> Self {
        self.disk_id = Some(disk_id);
        self
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
        self.disk_id
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
async fn write_read_4k_sector_disk_impl(driver: &DefaultDriver, backend: Backend) {
    // 64 KiB disk with 4096-byte sectors → 16 disk sectors.
    let BackendDisk { disk, _guard } = backend.make(driver, 64 * 1024, 4096, 4096, false).await;
    let mut harness = TestHarness::new(driver, disk, false);
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
async fn sector_shift_multiple_offsets_4k_impl(driver: &DefaultDriver, backend: Backend) {
    // 128 KiB disk with 4096-byte sectors → 32 disk sectors.
    let BackendDisk { disk, _guard } = backend.make(driver, 128 * 1024, 4096, 4096, false).await;
    let mut harness = TestHarness::new(driver, disk, false);
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
    backend_disk: BackendDisk,
    read_only: bool,
    sector: u64,
    num_sectors: u32,
    flags: u32,
    expected_status: u8,
) {
    let BackendDisk { disk, _guard } = backend_disk;
    let mut harness = TestHarness::new(driver, disk, read_only);
    harness.enable().await;
    let status_gpa = harness.post_discard_request(0, sector, num_sectors, flags);
    let (_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_len, 1);
    assert_eq!(harness.read_status(status_gpa), expected_status);
}

/// Discard with properly aligned sector and num_sectors on a 4K disk
/// should succeed.
async fn discard_aligned_succeeds_impl(driver: &DefaultDriver, backend: Backend) {
    // Discard virtio sector 8 (disk sector 1), num_sectors=8 (8×512 = 4096).
    check_discard(
        driver,
        backend.make(driver, 64 * 1024, 4096, 4096, false).await,
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
async fn discard_misaligned_num_sectors_fails_impl(driver: &DefaultDriver, backend: Backend) {
    // num_sectors=5 is not a multiple of 8 (4096/512).
    check_discard(
        driver,
        backend.make(driver, 64 * 1024, 4096, 4096, false).await,
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
async fn discard_misaligned_sector_fails_impl(driver: &DefaultDriver, backend: Backend) {
    // sector=3 is not aligned to 8 (4096/512).
    check_discard(
        driver,
        backend.make(driver, 64 * 1024, 4096, 4096, false).await,
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
async fn discard_with_unmap_flag_returns_unsupp_impl(driver: &DefaultDriver, backend: Backend) {
    check_discard(
        driver,
        backend.make(driver, 64 * 1024, 4096, 4096, false).await,
        false,
        0,
        8,
        1,
        VIRTIO_BLK_S_UNSUPP,
    )
    .await;
}

/// Discard on a read-only disk should fail with IOERR.
async fn discard_on_read_only_disk_fails_impl(driver: &DefaultDriver, backend: Backend) {
    check_discard(
        driver,
        backend.make(driver, 64 * 1024, 4096, 4096, true).await,
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
async fn discard_512b_sector_any_count_succeeds_impl(driver: &DefaultDriver, backend: Backend) {
    // sector_shift=0, sector_mask=0 → any num_sectors is "aligned".
    check_discard(
        driver,
        backend.make(driver, 64 * 1024, 512, 512, false).await,
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

// ---------------------------------------------------------------------------
// Rust VHDX backend tests
//
// These drive the virtio-blk device against the pure-Rust VHDX engine
// (`virtqueue -> virtio-blk -> DiskIo -> Rust VHDX`) instead of a RAM disk.
// Differencing-chain coverage requires parent-locator fixture setup
// that this harness does not yet provide and is tracked separately.
// ---------------------------------------------------------------------------

/// (Data Path, Fixed): write and read the first and last sectors of
/// a fixed image, and verify an untouched interior sector reads back zero
/// (fixed payload is zero-initialized).
#[async_test]
async fn vhdx_fixed_write_read_first_and_last_sectors(driver: DefaultDriver) {
    let (disk, _dir) = vhdx_disk(
        &driver,
        VhdxParams {
            disk_size: 64 * 1024, // 128 sectors
            block_size: 0,
            logical_sector_size: 512,
            physical_sector_size: 512,
            fixed: true,
            read_only: false,
        },
    )
    .await;
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.enable().await;

    let first: Vec<u8> = (0..512).map(|i| (i % 251) as u8).collect();
    let last: Vec<u8> = (0..512).map(|i| ((i * 3 + 7) % 251) as u8).collect();

    harness.post_write_request(0, 0, &first);
    harness.wait_for_used().await;
    harness.post_write_request(3, 127, &last);
    harness.wait_for_used().await;

    let gpa_first = harness.post_read_request(6, 0, 512);
    harness.wait_for_used().await;
    let gpa_last = harness.post_read_request(8, 127, 512);
    harness.wait_for_used().await;
    let gpa_mid = harness.post_read_request(10, 64, 512);
    harness.wait_for_used().await;

    let mut buf = vec![0u8; 512];
    harness.mem.read_at(gpa_first, &mut buf).unwrap();
    assert_eq!(buf, first, "sector 0 mismatch");
    harness.mem.read_at(gpa_last, &mut buf).unwrap();
    assert_eq!(buf, last, "last sector mismatch");
    harness.mem.read_at(gpa_mid, &mut buf).unwrap();
    assert!(
        buf.iter().all(|&b| b == 0),
        "interior sector should be zero"
    );
}

/// (Data Path, Dynamic): write across a block boundary and verify an
/// unallocated hole in an otherwise-touched block reads back zero.
#[async_test]
async fn vhdx_dynamic_block_boundary_and_holes(driver: DefaultDriver) {
    // 2 MiB disk, 1 MiB blocks => 2 blocks of 2048 sectors each.
    let (disk, _dir) = vhdx_disk(
        &driver,
        VhdxParams {
            disk_size: 2 * 1024 * 1024,
            block_size: 1024 * 1024,
            logical_sector_size: 512,
            physical_sector_size: 512,
            fixed: false,
            read_only: false,
        },
    )
    .await;
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.enable().await;

    let end_of_block0 = [0xA1u8; 512];
    let start_of_block1 = [0xB2u8; 512];

    harness.post_write_request(0, 2047, &end_of_block0); // last sector, block 0
    harness.wait_for_used().await;
    harness.post_write_request(3, 2048, &start_of_block1); // first sector, block 1
    harness.wait_for_used().await;

    let gpa0 = harness.post_read_request(6, 2047, 512);
    harness.wait_for_used().await;
    let gpa1 = harness.post_read_request(8, 2048, 512);
    harness.wait_for_used().await;
    let gpa_hole = harness.post_read_request(10, 3000, 512); // untouched hole in block 1
    harness.wait_for_used().await;

    let mut buf = vec![0u8; 512];
    harness.mem.read_at(gpa0, &mut buf).unwrap();
    assert_eq!(buf, end_of_block0, "last sector of block 0 mismatch");
    harness.mem.read_at(gpa1, &mut buf).unwrap();
    assert_eq!(buf, start_of_block1, "first sector of block 1 mismatch");
    harness.mem.read_at(gpa_hole, &mut buf).unwrap();
    assert!(
        buf.iter().all(|&b| b == 0),
        "unallocated hole should be zero"
    );
}

/// (Geometry, 512e): on a 512-logical / 4096-physical image, a single
/// 512-byte logical write must preserve neighboring logical sectors within the
/// same 4 KiB physical sector (read-modify-write correctness).
#[async_test]
async fn vhdx_512e_partial_physical_sector_preserves_neighbors(driver: DefaultDriver) {
    let (disk, _dir) = vhdx_disk(
        &driver,
        VhdxParams {
            disk_size: 64 * 1024,
            block_size: 0,
            logical_sector_size: 512,
            physical_sector_size: 4096,
            fixed: false,
            read_only: false,
        },
    )
    .await;
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.enable().await;

    // Fill one 4 KiB physical sector (8 logical sectors) with 0xAA.
    let fill = vec![0xAAu8; 4096];
    harness.post_write_request(0, 0, &fill);
    harness.wait_for_used().await;

    // Overwrite only logical sector 1 (512 bytes) with 0xBB.
    let partial = [0xBBu8; 512];
    harness.post_write_request(3, 1, &partial);
    harness.wait_for_used().await;

    // Read the whole physical sector back and check neighbor preservation.
    let gpa = harness.post_read_request(6, 0, 4096);
    harness.wait_for_used().await;
    let mut buf = vec![0u8; 4096];
    harness.mem.read_at(gpa, &mut buf).unwrap();
    for (sector, chunk) in buf.chunks_exact(512).enumerate() {
        let expected = if sector == 1 { 0xBB } else { 0xAA };
        assert!(
            chunk.iter().all(|&b| b == expected),
            "logical sector {sector} not preserved (expected 0x{expected:02x})"
        );
    }
}

/// (Space Management): a block-aligned DISCARD on a non-differencing
/// image zeroes the discarded range, preserves data in other blocks, and the
/// range remains writable afterwards (write-after-trim).
#[async_test]
async fn vhdx_discard_zeroes_block_and_preserves_neighbor(driver: DefaultDriver) {
    // 2 MiB disk, 1 MiB blocks => 2 blocks of 2048 sectors each.
    let (disk, _dir) = vhdx_disk(
        &driver,
        VhdxParams {
            disk_size: 2 * 1024 * 1024,
            block_size: 1024 * 1024,
            logical_sector_size: 512,
            physical_sector_size: 512,
            fixed: false,
            read_only: false,
        },
    )
    .await;
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.enable().await;

    harness.post_write_request(0, 0, &[0xCCu8; 512]); // block 0
    harness.wait_for_used().await;
    harness.post_write_request(3, 2048, &[0xDDu8; 512]); // block 1
    harness.wait_for_used().await;

    // Discard all of block 0 (sectors 0..2048).
    let status_gpa = harness.post_discard_request(6, 0, 2048, 0);
    let (_id, used_len) = harness.wait_for_used().await;
    assert_eq!(used_len, 1);
    assert_eq!(harness.read_status(status_gpa), VIRTIO_BLK_S_OK);

    // Discarded sector reads back zero; neighboring block is preserved.
    let gpa0 = harness.post_read_request(8, 0, 512);
    harness.wait_for_used().await;
    let gpa1 = harness.post_read_request(10, 2048, 512);
    harness.wait_for_used().await;
    let mut buf = [0u8; 512];
    harness.mem.read_at(gpa0, &mut buf).unwrap();
    assert!(
        buf.iter().all(|&b| b == 0),
        "discarded range should be zero"
    );
    harness.mem.read_at(gpa1, &mut buf).unwrap();
    assert!(
        buf.iter().all(|&b| b == 0xDD),
        "neighbor block mutated by discard"
    );

    // Write-after-trim: the discarded range is still writable.
    harness.post_write_request(12, 0, &[0xEEu8; 512]);
    harness.wait_for_used().await;
    let gpa2 = harness.post_read_request(15, 0, 512);
    harness.wait_for_used().await;
    harness.mem.read_at(gpa2, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0xEE), "write-after-trim failed");
}

/// (Durability): data written and FLUSHed through the virtio-blk
/// device must survive closing and reopening the VHDX file.
///
/// This is VHDX-only: a RAM disk cannot model persistence across reopen, so it
/// is not part of the shared `flush_succeeds` matrix.
#[async_test]
async fn vhdx_flush_persists_across_reopen(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.vhdx");
    let pattern: Vec<u8> = (0..512).map(|i| (i % 251) as u8).collect();

    // Phase 1: create, write sector 0 through virtio-blk, FLUSH, then close
    // (the writable disk is dropped at the end of this scope).
    {
        create_vhdx_file(&path, 64 * 1024).await;
        let disk = open_vhdx_file_disk(&driver, &path, false, false).await;
        let mut harness = TestHarness::new(&driver, disk, false);
        harness.enable().await;

        harness.post_write_request(0, 0, &pattern);
        harness.wait_for_used().await;

        harness.post_flush_request(3);
        let (_id, len) = harness.wait_for_used().await;
        assert_eq!(len, 1, "flush should complete with status only");
    }

    // Phase 2: reopen read-only (replaying any dirty log) and read sector 0.
    let disk = open_vhdx_file_disk(&driver, &path, true, true).await;
    let mut harness = TestHarness::new(&driver, disk, true);
    harness.enable().await;

    let gpa = harness.post_read_request(0, 0, 512);
    harness.wait_for_used().await;
    let mut readback = vec![0u8; 512];
    harness.mem.read_at(gpa, &mut readback).unwrap();
    assert_eq!(
        readback, pattern,
        "flushed data did not persist across reopen"
    );
}

/// (Validation): requests addressing sectors beyond the VHDX
/// capacity must fail with IOERR and must not corrupt in-range data.
#[async_test]
async fn vhdx_out_of_range_request_fails(driver: DefaultDriver) {
    // 64 KiB dynamic VHDX => 128 valid sectors (0..128).
    let BackendDisk { disk, _guard } = Backend::Vhdx
        .make(&driver, 64 * 1024, 512, 512, false)
        .await;
    let mut harness = TestHarness::new(&driver, disk, false);
    harness.enable().await;

    // Seed a known pattern at an in-range sector to detect corruption.
    let pattern = [0x5Au8; 512];
    harness.post_write_request(0, 0, &pattern);
    harness.wait_for_used().await;

    // Out-of-range READ (sector 500 >> 128) must fail with IOERR.
    let oob = 500u64;
    let data_gpa = harness.post_read_request(3, oob, 512);
    harness.wait_for_used().await;
    assert_eq!(
        harness.read_status(data_gpa + 512),
        VIRTIO_BLK_S_IOERR,
        "out-of-range read should fail with IOERR"
    );

    // Out-of-range WRITE must fail with IOERR (manual chain to capture status).
    let header_gpa = harness.alloc_data(REQ_HEADER_SIZE);
    let wdata_gpa = harness.alloc_data(512);
    let status_gpa = harness.alloc_data(1);
    let header = VirtioBlkReqHeader {
        request_type: VIRTIO_BLK_T_OUT,
        reserved: 0,
        sector: oob,
    };
    harness.mem.write_at(header_gpa, header.as_bytes()).unwrap();
    harness.mem.write_at(wdata_gpa, &[0xEEu8; 512]).unwrap();
    harness.mem.write_at(status_gpa, &[0xFFu8]).unwrap();
    write_descriptor(
        &harness.mem,
        DESC_ADDR,
        5,
        header_gpa,
        REQ_HEADER_SIZE,
        DescriptorFlags::new().with_next(true),
        6,
    );
    write_descriptor(
        &harness.mem,
        DESC_ADDR,
        6,
        wdata_gpa,
        512,
        DescriptorFlags::new().with_next(true),
        7,
    );
    write_descriptor(
        &harness.mem,
        DESC_ADDR,
        7,
        status_gpa,
        1,
        DescriptorFlags::new().with_write(true),
        0,
    );
    make_available(
        &harness.mem,
        AVAIL_ADDR,
        QUEUE_SIZE,
        5,
        &mut harness.avail_idx,
    );
    harness.queue_event.signal();
    harness.wait_for_used().await;
    assert_eq!(
        harness.read_status(status_gpa),
        VIRTIO_BLK_S_IOERR,
        "out-of-range write should fail with IOERR"
    );

    // In-range data must be unchanged by the failed out-of-range operations.
    let check_gpa = harness.post_read_request(9, 0, 512);
    harness.wait_for_used().await;
    let mut buf = [0u8; 512];
    harness.mem.read_at(check_gpa, &mut buf).unwrap();
    assert_eq!(buf, pattern, "in-range data corrupted by out-of-range op");
}

/// (Validation): opening a malformed/truncated image must fail
/// cleanly (no panic), both read-only and writable.
#[async_test]
async fn vhdx_malformed_image_open_fails(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.vhdx");
    // Garbage content: no valid VHDX file identifier / headers.
    std::fs::write(&path, vec![0xFFu8; 64 * 1024]).unwrap();

    let bf = BlockingFile::open(&path, false).unwrap();
    assert!(
        VhdxFile::open(bf).read_only().await.is_err(),
        "malformed image must fail read-only open"
    );

    let bf = BlockingFile::open(&path, false).unwrap();
    assert!(
        VhdxFile::open(bf).writable(&driver).await.is_err(),
        "malformed image must fail writable open"
    );
}

/// (Validation): after an unclean shutdown the VHDX log is dirty; a
/// read-only open without replay must be rejected, and opening with replay must
/// recover the committed data.
#[async_test]
async fn vhdx_dirty_log_requires_replay(driver: DefaultDriver) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.vhdx");
    create_vhdx_file(&path, 64 * 1024).await;
    let pattern = [0xC7u8; 512];

    // Phase 1: write + flush through the device, then drop WITHOUT a clean
    // close, leaving the log dirty (simulates an unclean shutdown).
    {
        let disk = open_vhdx_file_disk(&driver, &path, false, false).await;
        let mut harness = TestHarness::new(&driver, disk, false);
        harness.enable().await;
        harness.post_write_request(0, 0, &pattern);
        harness.wait_for_used().await;
        harness.post_flush_request(3);
        harness.wait_for_used().await;
        // dropped here: no close() => dirty log on disk.
    }

    // Phase 2: a read-only open WITHOUT replay must be rejected.
    {
        let bf = BlockingFile::open(&path, false).unwrap();
        assert!(
            VhdxFile::open(bf).read_only().await.is_err(),
            "dirty-log read-only open without replay must fail"
        );
    }

    // Phase 3: opening with replay recovers the committed data.
    let disk = open_vhdx_file_disk(&driver, &path, true, true).await;
    let mut harness = TestHarness::new(&driver, disk, true);
    harness.enable().await;
    let gpa = harness.post_read_request(0, 0, 512);
    harness.wait_for_used().await;
    let mut buf = [0u8; 512];
    harness.mem.read_at(gpa, &mut buf).unwrap();
    assert_eq!(buf, pattern, "data not recovered after log replay");
}
