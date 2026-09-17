// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the virtio-fs device.
//!
//! These tests construct a full `VirtioFsDevice` backed by a real temp
//! directory, then drive FUSE requests through the descriptor ring just
//! as a guest kernel would.

use crate::VirtioFs;
use crate::virtio::VirtioFsDevice;
use fuse::protocol::*;
use guestmem::GuestMemory;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_event::Event;
use test_with_tracing::test;
use virtio::QueueResources;
use virtio::VirtioDevice;
use virtio::queue::QueueParams;
use virtio::spec::VirtioDeviceFeatures;
use virtio::spec::queue::DescriptorFlags;
use virtio::test_helpers::init_avail_ring;
use virtio::test_helpers::init_used_ring;
use virtio::test_helpers::make_available;
use virtio::test_helpers::wait_for_used;
use virtio::test_helpers::write_descriptor;
use vmcore::interrupt::Interrupt;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

// --- Constants ---

const QUEUE_SIZE: u16 = 16;

const DESC_ADDR: u64 = 0x0000;
const AVAIL_ADDR: u64 = 0x1000;
const USED_ADDR: u64 = 0x2000;

const DATA_BASE: u64 = 0x10000;
const TOTAL_MEM_SIZE: usize = 0x40000;

// FUSE header sizes
const IN_HEADER_SIZE: u32 = size_of::<fuse_in_header>() as u32;
const OUT_HEADER_SIZE: u32 = size_of::<fuse_out_header>() as u32;

// --- Test Harness ---

struct TestHarness {
    device: VirtioFsDevice,
    mem: GuestMemory,
    driver: DefaultDriver,
    queue_event: Event,
    interrupt_event: Event,
    avail_idx: u16,
    used_idx: u16,
    next_data_offset: u64,
    next_unique: u64,
    _tmpdir: tempfile::TempDir,
}

impl TestHarness {
    fn new(driver: &DefaultDriver) -> Self {
        let tmpdir = tempfile::tempdir().unwrap();

        let mem = GuestMemory::allocate(TOTAL_MEM_SIZE);
        init_avail_ring(&mem, AVAIL_ADDR);
        init_used_ring(&mem, USED_ADDR);

        let fs = VirtioFs::new(tmpdir.path(), None).unwrap();
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
        let device = VirtioFsDevice::new(&driver_source, "testfs", fs, 0, None);

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
            next_unique: 1,
            _tmpdir: tmpdir,
        }
    }

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

    fn alloc_data(&mut self, size: u32) -> u64 {
        let gpa = self.next_data_offset;
        self.next_data_offset += size as u64;
        assert!(
            self.next_data_offset <= TOTAL_MEM_SIZE as u64,
            "ran out of test memory"
        );
        gpa
    }

    fn next_unique(&mut self) -> u64 {
        let u = self.next_unique;
        self.next_unique += 1;
        u
    }

    /// Post a FUSE request with a readable descriptor (header + args) and a
    /// writable descriptor (response buffer). Returns `(unique, resp_gpa)` —
    /// the request's unique ID and the GPA of the response buffer.
    fn post_fuse_request(
        &mut self,
        head_desc: u16,
        opcode: u32,
        nodeid: u64,
        args: &[u8],
        response_buf_size: u32,
    ) -> (u64, u64) {
        let unique = self.next_unique();
        let total_in_len = IN_HEADER_SIZE + args.len() as u32;
        let req_gpa = self.alloc_data(total_in_len);
        let resp_gpa = self.alloc_data(response_buf_size);

        // Write the FUSE in_header + args
        let header = fuse_in_header {
            len: total_in_len,
            opcode,
            unique,
            nodeid,
            uid: 0,
            gid: 0,
            pid: 1,
            padding: 0,
        };
        self.mem.write_at(req_gpa, header.as_bytes()).unwrap();
        if !args.is_empty() {
            self.mem
                .write_at(req_gpa + IN_HEADER_SIZE as u64, args)
                .unwrap();
        }

        // Zero the response buffer
        let zeroes = vec![0u8; response_buf_size as usize];
        self.mem.write_at(resp_gpa, &zeroes).unwrap();

        // desc 0: request (readable)
        let flags0 = DescriptorFlags::new().with_next(true);
        write_descriptor(
            &self.mem,
            DESC_ADDR,
            head_desc,
            req_gpa,
            total_in_len,
            flags0,
            head_desc + 1,
        );

        // desc 1: response (writable)
        let flags1 = DescriptorFlags::new().with_write(true);
        write_descriptor(
            &self.mem,
            DESC_ADDR,
            head_desc + 1,
            resp_gpa,
            response_buf_size,
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

        (unique, resp_gpa)
    }

    /// Post a FUSE request that expects no reply (e.g. FORGET).
    /// Uses a single readable descriptor with no writable descriptor.
    fn post_fuse_no_reply(&mut self, head_desc: u16, opcode: u32, nodeid: u64, args: &[u8]) {
        let unique = self.next_unique();
        let total_in_len = IN_HEADER_SIZE + args.len() as u32;
        let req_gpa = self.alloc_data(total_in_len);

        let header = fuse_in_header {
            len: total_in_len,
            opcode,
            unique,
            nodeid,
            uid: 0,
            gid: 0,
            pid: 1,
            padding: 0,
        };
        self.mem.write_at(req_gpa, header.as_bytes()).unwrap();
        if !args.is_empty() {
            self.mem
                .write_at(req_gpa + IN_HEADER_SIZE as u64, args)
                .unwrap();
        }

        // Single readable descriptor, no writable descriptor
        let flags = DescriptorFlags::new();
        write_descriptor(
            &self.mem,
            DESC_ADDR,
            head_desc,
            req_gpa,
            total_in_len,
            flags,
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

    /// Read the FUSE out_header from a response buffer GPA.
    fn read_out_header(&self, resp_gpa: u64) -> fuse_out_header {
        let mut buf = [0u8; size_of::<fuse_out_header>()];
        self.mem.read_at(resp_gpa, &mut buf).unwrap();
        fuse_out_header::read_from_bytes(&buf).unwrap()
    }

    /// Read a response payload (after the out_header) from a response buffer.
    fn read_response<T: FromBytes>(&self, resp_gpa: u64) -> T {
        let offset = size_of::<fuse_out_header>() as u64;
        let mut buf = vec![0u8; size_of::<T>()];
        self.mem.read_at(resp_gpa + offset, &mut buf).unwrap();
        T::read_from_bytes(&buf).unwrap()
    }

    /// Send FUSE_INIT and wait for the response. Panics on failure.
    async fn fuse_init(&mut self, head_desc: u16) {
        let init_args = fuse_init_in {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: 0,
            flags: 0,
            flags2: 0,
            unused: [0; 11],
        };

        let resp_size = OUT_HEADER_SIZE + size_of::<fuse_init_out>() as u32;
        let (unique, resp_gpa) =
            self.post_fuse_request(head_desc, FUSE_INIT, 0, init_args.as_bytes(), resp_size);

        let (_used_id, used_len) = self.wait_for_used().await;
        assert!(used_len > 0, "FUSE_INIT response should not be empty");

        let out_header = self.read_out_header(resp_gpa);
        assert_eq!(out_header.unique, unique);
        assert_eq!(out_header.error, 0, "FUSE_INIT failed");

        let init_out: fuse_init_out = self.read_response(resp_gpa);
        assert_eq!(init_out.major, FUSE_KERNEL_VERSION);
    }

    /// Return the path to the temp directory backing the filesystem.
    fn tmpdir_path(&self) -> &std::path::Path {
        self._tmpdir.path()
    }
}

// --- Tests ---

/// FUSE_INIT handshake succeeds and returns the correct protocol version.
#[async_test]
async fn fuse_init_succeeds(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;
    harness.fuse_init(0).await;
}

/// GETATTR on the root inode returns a directory after INIT.
#[async_test]
async fn getattr_root_returns_directory(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;
    harness.fuse_init(0).await;

    let getattr_args = fuse_getattr_in {
        getattr_flags: 0,
        dummy: 0,
        fh: 0,
    };

    let resp_size = OUT_HEADER_SIZE + size_of::<fuse_attr_out>() as u32;
    let (unique, resp_gpa) = harness.post_fuse_request(
        2,
        FUSE_GETATTR,
        FUSE_ROOT_ID,
        getattr_args.as_bytes(),
        resp_size,
    );

    let (_used_id, used_len) = harness.wait_for_used().await;
    assert!(used_len > 0);

    let out_header = harness.read_out_header(resp_gpa);
    assert_eq!(out_header.unique, unique);
    assert_eq!(out_header.error, 0, "GETATTR on root failed");

    let attr_out: fuse_attr_out = harness.read_response(resp_gpa);
    // S_IFDIR = 0o040000
    assert_eq!(
        attr_out.attr.mode & 0o170000,
        0o040000,
        "root inode should be a directory"
    );
}

/// FUSE_FORGET is a no-reply operation — the descriptor should still be
/// completed (with 0 bytes written) so the virtqueue doesn't stall.
///
/// This exercises the path that currently relies on
/// `VirtioQueueCallbackWork::Drop` auto-completing the descriptor.
#[async_test]
async fn forget_completes_descriptor(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;
    harness.fuse_init(0).await;

    // FORGET requires a valid nodeid that has been looked up.
    // The root inode (FUSE_ROOT_ID=1) always exists, so use it.
    let forget_args = fuse_forget_in { nlookup: 0 };

    harness.post_fuse_no_reply(2, FUSE_FORGET, FUSE_ROOT_ID, forget_args.as_bytes());

    let (_used_id, used_len) = harness.wait_for_used().await;
    // FORGET has no reply, so the device should complete with 0 bytes.
    assert_eq!(used_len, 0, "FORGET should complete with 0 bytes written");
}

/// A malformed FUSE request (header too short) should complete the
/// descriptor rather than hanging the queue.
#[async_test]
async fn malformed_request_completes_descriptor(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;

    // Post a descriptor with garbage data (not a valid FUSE header).
    let garbage = [0xFFu8; 4]; // Too short to be a fuse_in_header
    let req_gpa = harness.alloc_data(garbage.len() as u32);
    harness.mem.write_at(req_gpa, &garbage).unwrap();

    let resp_size = 256u32;
    let resp_gpa = harness.alloc_data(resp_size);
    harness
        .mem
        .write_at(resp_gpa, &vec![0u8; resp_size as usize])
        .unwrap();

    // desc 0: garbage request (readable)
    let flags0 = DescriptorFlags::new().with_next(true);
    write_descriptor(
        &harness.mem,
        DESC_ADDR,
        0,
        req_gpa,
        garbage.len() as u32,
        flags0,
        1,
    );

    // desc 1: response buffer (writable)
    let flags1 = DescriptorFlags::new().with_write(true);
    write_descriptor(&harness.mem, DESC_ADDR, 1, resp_gpa, resp_size, flags1, 0);

    make_available(
        &harness.mem,
        AVAIL_ADDR,
        QUEUE_SIZE,
        0,
        &mut harness.avail_idx,
    );
    harness.queue_event.signal();

    let (_used_id, used_len) = harness.wait_for_used().await;
    // The device should complete the descriptor (possibly with 0 bytes)
    // rather than hanging.
    let _ = used_len; // Any completion is acceptable.
}

/// LOOKUP on a file that exists in the temp directory succeeds.
#[async_test]
async fn lookup_existing_file(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);

    // Create a file in the temp dir before booting the device.
    std::fs::write(harness.tmpdir_path().join("hello.txt"), "test data").unwrap();

    harness.enable().await;
    harness.fuse_init(0).await;

    // FUSE_LOOKUP: the "args" is a null-terminated filename after the header.
    let name = b"hello.txt\0";
    let resp_size = OUT_HEADER_SIZE + size_of::<fuse_entry_out>() as u32;
    let (unique, resp_gpa) =
        harness.post_fuse_request(2, FUSE_LOOKUP, FUSE_ROOT_ID, name, resp_size);

    let (_used_id, used_len) = harness.wait_for_used().await;
    assert!(used_len > 0);

    let out_header = harness.read_out_header(resp_gpa);
    assert_eq!(out_header.unique, unique);
    assert_eq!(
        out_header.error, 0,
        "LOOKUP should succeed for existing file"
    );

    let entry_out: fuse_entry_out = harness.read_response(resp_gpa);
    assert_ne!(entry_out.nodeid, 0, "returned nodeid should be non-zero");
    // S_IFREG = 0o100000
    assert_eq!(
        entry_out.attr.mode & 0o170000,
        0o100000,
        "hello.txt should be a regular file"
    );
}

/// LOOKUP on a non-existent file returns ENOENT.
#[async_test]
async fn lookup_nonexistent_returns_enoent(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;
    harness.fuse_init(0).await;

    let name = b"does_not_exist.txt\0";
    let resp_size = OUT_HEADER_SIZE + size_of::<fuse_entry_out>() as u32;
    let (unique, resp_gpa) =
        harness.post_fuse_request(2, FUSE_LOOKUP, FUSE_ROOT_ID, name, resp_size);

    let (_used_id, used_len) = harness.wait_for_used().await;
    assert!(used_len > 0);

    let out_header = harness.read_out_header(resp_gpa);
    assert_eq!(out_header.unique, unique);
    // ENOENT = -2
    assert_eq!(
        out_header.error, -2,
        "LOOKUP should return ENOENT for missing file"
    );
}

/// When the kernel advertises FUSE_INIT_EXT and FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2,
/// VirtioFs::init should request that flag and the INIT response should include
/// it in flags2.
#[async_test]
async fn init_negotiates_direct_io_allow_mmap(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;

    // Send FUSE_INIT with FUSE_INIT_EXT set in flags and
    // FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2 advertised in flags2.
    let init_args = fuse_init_in {
        major: FUSE_KERNEL_VERSION,
        minor: FUSE_KERNEL_MINOR_VERSION,
        max_readahead: 131072,
        flags: FUSE_INIT_EXT,
        flags2: FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2,
        unused: [0; 11],
    };

    let resp_size = OUT_HEADER_SIZE + size_of::<fuse_init_out>() as u32;
    let (unique, resp_gpa) =
        harness.post_fuse_request(0, FUSE_INIT, 0, init_args.as_bytes(), resp_size);

    let (_used_id, used_len) = harness.wait_for_used().await;
    assert!(used_len > 0, "FUSE_INIT response should not be empty");

    let out_header = harness.read_out_header(resp_gpa);
    assert_eq!(out_header.unique, unique);
    assert_eq!(out_header.error, 0, "FUSE_INIT failed");

    let init_out: fuse_init_out = harness.read_response(resp_gpa);
    assert_eq!(init_out.major, FUSE_KERNEL_VERSION);
    assert_ne!(
        init_out.flags & FUSE_INIT_EXT,
        0,
        "response should include FUSE_INIT_EXT in flags"
    );
    assert_ne!(
        init_out.flags2 & FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2,
        0,
        "VirtioFs should request FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2 when kernel advertises it"
    );
}
