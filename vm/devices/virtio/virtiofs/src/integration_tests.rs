// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the virtio-fs device.
//!
//! These tests construct a full `VirtioFsDevice` backed by a real temp
//! directory, then drive FUSE requests through the descriptor ring just
//! as a guest kernel would.

use crate::LxVolumeOptions;
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

/// Shared FUSE virtqueue plumbing. Holds the device, guest memory, queue
/// rings, and helpers for posting/reading FUSE requests. Used by both the
/// single-root and aggregate test harnesses via `Deref`/`DerefMut`.
struct FuseRing {
    device: VirtioFsDevice,
    mem: GuestMemory,
    driver: DefaultDriver,
    queue_event: Event,
    interrupt_event: Event,
    avail_idx: u16,
    used_idx: u16,
    next_data_offset: u64,
    next_unique: u64,
}

impl FuseRing {
    fn new(driver: &DefaultDriver, fs: VirtioFs, name: &str) -> Self {
        let mem = GuestMemory::allocate(TOTAL_MEM_SIZE);
        init_avail_ring(&mem, AVAIL_ADDR);
        init_used_ring(&mem, USED_ADDR);
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
        let device = VirtioFsDevice::new(&driver_source, name, fs, 0, None);
        Self {
            device,
            mem,
            driver: driver.clone(),
            queue_event: Event::new(),
            interrupt_event: Event::new(),
            avail_idx: 0,
            used_idx: 0,
            next_data_offset: DATA_BASE,
            next_unique: 1,
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

    /// Post a FUSE request with a readable descriptor (header + args) and
    /// a writable descriptor (response buffer). Returns
    /// `(unique, resp_gpa)`.
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

        let zeroes = vec![0u8; response_buf_size as usize];
        self.mem.write_at(resp_gpa, &zeroes).unwrap();

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

    /// Post a FUSE request that expects no reply (e.g. FORGET) using a
    /// single readable descriptor.
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

    fn read_out_header(&self, resp_gpa: u64) -> fuse_out_header {
        let mut buf = [0u8; size_of::<fuse_out_header>()];
        self.mem.read_at(resp_gpa, &mut buf).unwrap();
        fuse_out_header::read_from_bytes(&buf).unwrap()
    }

    fn read_response<T: FromBytes>(&self, resp_gpa: u64) -> T {
        let offset = size_of::<fuse_out_header>() as u64;
        let mut buf = vec![0u8; size_of::<T>()];
        self.mem.read_at(resp_gpa + offset, &mut buf).unwrap();
        T::read_from_bytes(&buf).unwrap()
    }

    /// Send FUSE_INIT with `flags`/`flags2` and wait for success. Returns
    /// the device's `fuse_init_out` response.
    async fn fuse_init_with(&mut self, head_desc: u16, flags: u32, flags2: u32) -> fuse_init_out {
        let init_args = fuse_init_in {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: 0,
            flags,
            flags2,
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
        init_out
    }

    /// Send a plain FUSE_INIT (no flags) and wait for success.
    async fn fuse_init(&mut self, head_desc: u16) {
        let _ = self.fuse_init_with(head_desc, 0, 0).await;
    }

    /// Issue FUSE_LOOKUP for `name` under `parent_nodeid`. Returns the
    /// `fuse_entry_out`. Panics if lookup fails.
    async fn lookup_child(
        &mut self,
        head_desc: u16,
        parent_nodeid: u64,
        name: &[u8],
    ) -> fuse_entry_out {
        let mut name_bytes = name.to_vec();
        name_bytes.push(0);
        let resp_size = OUT_HEADER_SIZE + size_of::<fuse_entry_out>() as u32;
        let (unique, resp_gpa) = self.post_fuse_request(
            head_desc,
            FUSE_LOOKUP,
            parent_nodeid,
            &name_bytes,
            resp_size,
        );
        let (_used_id, _used_len) = self.wait_for_used().await;
        let out_header = self.read_out_header(resp_gpa);
        assert_eq!(out_header.unique, unique);
        assert_eq!(out_header.error, 0, "LOOKUP failed");
        self.read_response::<fuse_entry_out>(resp_gpa)
    }
}

struct TestHarness {
    ring: FuseRing,
    _tmpdir: tempfile::TempDir,
}

impl TestHarness {
    fn new(driver: &DefaultDriver) -> Self {
        let tmpdir = tempfile::tempdir().unwrap();
        let fs = VirtioFs::new(tmpdir.path(), None).unwrap();
        Self {
            ring: FuseRing::new(driver, fs, "testfs"),
            _tmpdir: tmpdir,
        }
    }

    fn tmpdir_path(&self) -> &std::path::Path {
        self._tmpdir.path()
    }
}

impl std::ops::Deref for TestHarness {
    type Target = FuseRing;
    fn deref(&self) -> &FuseRing {
        &self.ring
    }
}

impl std::ops::DerefMut for TestHarness {
    fn deref_mut(&mut self) -> &mut FuseRing {
        &mut self.ring
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

    // Use the inner ring directly so we can split borrows over distinct fields.
    let ring = &mut harness.ring;

    // Post a descriptor with garbage data (not a valid FUSE header).
    let garbage = [0xFFu8; 4]; // Too short to be a fuse_in_header
    let req_gpa = ring.alloc_data(garbage.len() as u32);
    ring.mem.write_at(req_gpa, &garbage).unwrap();

    let resp_size = 256u32;
    let resp_gpa = ring.alloc_data(resp_size);
    ring.mem
        .write_at(resp_gpa, &vec![0u8; resp_size as usize])
        .unwrap();

    // desc 0: garbage request (readable)
    let flags0 = DescriptorFlags::new().with_next(true);
    write_descriptor(
        &ring.mem,
        DESC_ADDR,
        0,
        req_gpa,
        garbage.len() as u32,
        flags0,
        1,
    );

    // desc 1: response buffer (writable)
    let flags1 = DescriptorFlags::new().with_write(true);
    write_descriptor(&ring.mem, DESC_ADDR, 1, resp_gpa, resp_size, flags1, 0);

    make_available(&ring.mem, AVAIL_ADDR, QUEUE_SIZE, 0, &mut ring.avail_idx);
    ring.queue_event.signal();

    let (_used_id, used_len) = ring.wait_for_used().await;
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

/// Repeated LOOKUP of the same path on a stable-id volume must return the
/// same FUSE node id, proving the inode map deduplicates by identity. A
/// stable node id is what the guest relies on for inode-keyed features such
/// as inotify; the aggregate/FAT path achieves the same via path-based
/// dedup, which can only be exercised against a real FAT volume.
#[async_test]
async fn repeated_lookup_returns_stable_nodeid(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    std::fs::write(harness.tmpdir_path().join("hello.txt"), "test data").unwrap();

    harness.enable().await;
    harness.fuse_init(0).await;

    let first = harness.lookup_child(2, FUSE_ROOT_ID, b"hello.txt").await;
    let second = harness.lookup_child(2, FUSE_ROOT_ID, b"hello.txt").await;
    assert_ne!(first.nodeid, 0, "returned nodeid should be non-zero");
    assert_eq!(
        first.nodeid, second.nodeid,
        "repeated lookup of the same path must return a stable node id"
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
    let init_out = harness
        .fuse_init_with(0, FUSE_INIT_EXT, FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2)
        .await;
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

// --- Aggregate (multi-path) tests ---

/// Aggregate-virtio-fs harness: wraps a [`FuseRing`] with multiple backing
/// tempdirs and a live [`crate::VirtiofsAggregateHandle`] for tests that
/// exercise post-construction `add_child`.
struct AggregateTestHarness {
    ring: FuseRing,
    tmpdirs: Vec<tempfile::TempDir>,
    aggregate_handle: crate::VirtiofsAggregateHandle,
}

impl AggregateTestHarness {
    fn new(driver: &DefaultDriver, child_names: &[&str]) -> Self {
        let mut tmpdirs = Vec::with_capacity(child_names.len());
        let mut children = Vec::with_capacity(child_names.len());
        for name in child_names {
            let tmpdir = tempfile::tempdir().unwrap();
            children.push(crate::VirtioFsChild {
                name: (*name).to_string(),
                root_path: tmpdir.path().to_path_buf(),
                options: None,
            });
            tmpdirs.push(tmpdir);
        }
        let fs = VirtioFs::new_aggregate(children).unwrap();
        let aggregate_handle = fs.aggregate_handle().unwrap();
        Self {
            ring: FuseRing::new(driver, fs, "aggfs"),
            tmpdirs,
            aggregate_handle,
        }
    }

    /// Allocate a fresh tempdir owned by this harness so backing storage
    /// for a dynamically-added child lives until the harness drops.
    fn alloc_child_tmpdir(&mut self) -> std::path::PathBuf {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        self.tmpdirs.push(dir);
        path
    }

    /// Build a harness where every named child is backed by the *same* host
    /// directory. Used to prove per-share inode namespacing: the children
    /// stat identical underlying inodes, so any difference in the reported
    /// `st_ino` comes purely from namespacing.
    fn new_shared_backing(driver: &DefaultDriver, child_names: &[&str]) -> Self {
        let tmpdir = tempfile::tempdir().unwrap();
        let children = child_names
            .iter()
            .map(|name| crate::VirtioFsChild {
                name: (*name).to_string(),
                root_path: tmpdir.path().to_path_buf(),
                options: None,
            })
            .collect();
        let fs = VirtioFs::new_aggregate(children).unwrap();
        let aggregate_handle = fs.aggregate_handle().unwrap();
        Self {
            ring: FuseRing::new(driver, fs, "aggfs"),
            tmpdirs: vec![tmpdir],
            aggregate_handle,
        }
    }
}

impl std::ops::Deref for AggregateTestHarness {
    type Target = FuseRing;
    fn deref(&self) -> &FuseRing {
        &self.ring
    }
}

impl std::ops::DerefMut for AggregateTestHarness {
    fn deref_mut(&mut self) -> &mut FuseRing {
        &mut self.ring
    }
}

/// FUSE_INIT with FUSE_SUBMOUNTS advertised: VirtioFs::init should request
/// the feature back in the response so the kernel will honor
/// FUSE_ATTR_SUBMOUNT.
#[async_test]
async fn aggregate_init_negotiates_submounts(driver: DefaultDriver) {
    let mut harness = AggregateTestHarness::new(&driver, &["alpha", "beta"]);
    harness.enable().await;
    let init_out = harness.fuse_init_with(0, FUSE_SUBMOUNTS, 0).await;
    assert_ne!(
        init_out.flags & FUSE_SUBMOUNTS,
        0,
        "VirtioFs should accept FUSE_SUBMOUNTS when the kernel advertises it"
    );
}

/// GETATTR on the synthetic root reports a directory with nlink = 2 +
/// number of children.
#[async_test]
async fn aggregate_root_getattr_returns_directory(driver: DefaultDriver) {
    let mut harness = AggregateTestHarness::new(&driver, &["alpha", "beta", "gamma"]);
    harness.enable().await;
    harness.fuse_init(0).await;

    let getattr_args = fuse_getattr_in {
        getattr_flags: 0,
        dummy: 0,
        fh: 0,
    };
    let resp_size = OUT_HEADER_SIZE + size_of::<fuse_attr_out>() as u32;
    let (_unique, resp_gpa) = harness.post_fuse_request(
        2,
        FUSE_GETATTR,
        FUSE_ROOT_ID,
        getattr_args.as_bytes(),
        resp_size,
    );
    let (_used_id, _used_len) = harness.wait_for_used().await;
    let out_header = harness.read_out_header(resp_gpa);
    assert_eq!(out_header.error, 0, "GETATTR on aggregate root failed");
    let attr_out: fuse_attr_out = harness.read_response(resp_gpa);
    assert_eq!(
        attr_out.attr.mode & 0o170000,
        0o040000,
        "synthetic root should be a directory"
    );
    assert_eq!(attr_out.attr.ino, FUSE_ROOT_ID);
    assert_eq!(
        attr_out.attr.nlink, 5,
        "nlink should be 2 + child count (3) = 5"
    );
}

/// LOOKUP of a named child returns a directory inode with
/// FUSE_ATTR_SUBMOUNT set in attr.flags, so the kernel auto-mounts it.
#[async_test]
async fn aggregate_lookup_child_sets_submount_flag(driver: DefaultDriver) {
    let mut harness = AggregateTestHarness::new(&driver, &["alpha", "beta"]);
    harness.enable().await;
    harness.fuse_init(0).await;

    let entry = harness.lookup_child(2, FUSE_ROOT_ID, b"alpha").await;
    assert_ne!(entry.nodeid, 0, "child nodeid must be non-zero");
    assert_ne!(
        entry.nodeid, FUSE_ROOT_ID,
        "child nodeid must differ from root"
    );
    assert_eq!(
        entry.attr.mode & 0o170000,
        0o040000,
        "child root should be a directory"
    );
    assert_ne!(
        entry.attr.flags & FUSE_ATTR_SUBMOUNT,
        0,
        "child lookup must set FUSE_ATTR_SUBMOUNT so the kernel auto-mounts the subtree"
    );
}

/// Two aggregate children backed by the *same* host directory must report
/// *distinct* inode numbers for their (identical) submount roots, proving
/// per-share inode namespacing prevents cross-share `(st_dev, st_ino)`
/// collisions under the single shared superblock. The reported number must
/// also be stable across repeated lookups of the same child.
#[async_test]
async fn aggregate_children_namespace_inode_numbers(driver: DefaultDriver) {
    let mut harness = AggregateTestHarness::new_shared_backing(&driver, &["alpha", "beta"]);
    harness.enable().await;
    harness.fuse_init(0).await;

    let alpha = harness.lookup_child(2, FUSE_ROOT_ID, b"alpha").await;
    let beta = harness.lookup_child(2, FUSE_ROOT_ID, b"beta").await;

    // Both submount roots stat the same physical directory, so any
    // difference in the reported inode number comes purely from namespacing.
    assert_ne!(
        alpha.attr.ino, beta.attr.ino,
        "children backed by the same directory must get distinct namespaced inode numbers"
    );

    let alpha_again = harness.lookup_child(2, FUSE_ROOT_ID, b"alpha").await;
    assert_eq!(
        alpha.attr.ino, alpha_again.attr.ino,
        "repeated lookup of the same child must report a stable inode number"
    );
}

/// The inode number reported for a child's submount root via LOOKUP must
/// match the one reported via GETATTR on the same nodeid, i.e. namespacing
/// is applied consistently across reply paths.
#[async_test]
async fn aggregate_child_inode_number_consistent_across_lookup_and_getattr(driver: DefaultDriver) {
    let mut harness = AggregateTestHarness::new(&driver, &["alpha", "beta"]);
    harness.enable().await;
    harness.fuse_init(0).await;

    let entry = harness.lookup_child(2, FUSE_ROOT_ID, b"alpha").await;

    let getattr_args = fuse_getattr_in {
        getattr_flags: 0,
        dummy: 0,
        fh: 0,
    };
    let resp_size = OUT_HEADER_SIZE + size_of::<fuse_attr_out>() as u32;
    let (_unique, resp_gpa) = harness.post_fuse_request(
        2,
        FUSE_GETATTR,
        entry.nodeid,
        getattr_args.as_bytes(),
        resp_size,
    );
    let (_used_id, _used_len) = harness.wait_for_used().await;
    let out_header = harness.read_out_header(resp_gpa);
    assert_eq!(out_header.error, 0, "GETATTR on child failed");
    let attr_out: fuse_attr_out = harness.read_response(resp_gpa);
    assert_eq!(
        entry.attr.ino, attr_out.attr.ino,
        "LOOKUP and GETATTR must agree on the namespaced inode number"
    );
}

/// LOOKUP of a non-existent child returns ENOENT.
#[async_test]
async fn aggregate_lookup_missing_child_returns_enoent(driver: DefaultDriver) {
    let mut harness = AggregateTestHarness::new(&driver, &["alpha", "beta"]);
    harness.enable().await;
    harness.fuse_init(0).await;

    let name = b"does_not_exist\0";
    let resp_size = OUT_HEADER_SIZE + size_of::<fuse_entry_out>() as u32;
    let (_unique, resp_gpa) =
        harness.post_fuse_request(2, FUSE_LOOKUP, FUSE_ROOT_ID, name, resp_size);
    let (_used_id, _used_len) = harness.wait_for_used().await;
    let out_header = harness.read_out_header(resp_gpa);
    assert_eq!(
        out_header.error, -2,
        "LOOKUP on missing aggregate child should return ENOENT"
    );
}

/// Mutating operations on the synthetic root must fail with EROFS even
/// though the children themselves are writable, because the synthetic
/// root is purely a navigation layer (it has nowhere to put new files).
#[async_test]
async fn aggregate_mkdir_on_root_returns_erofs(driver: DefaultDriver) {
    let mut harness = AggregateTestHarness::new(&driver, &["alpha", "beta"]);
    harness.enable().await;
    harness.fuse_init(0).await;

    let mkdir_args = fuse_mkdir_in {
        mode: 0o755,
        umask: 0,
    };
    let mut args = mkdir_args.as_bytes().to_vec();
    args.extend_from_slice(b"new_dir\0");

    let resp_size = OUT_HEADER_SIZE + size_of::<fuse_entry_out>() as u32;
    let (_unique, resp_gpa) =
        harness.post_fuse_request(2, FUSE_MKDIR, FUSE_ROOT_ID, &args, resp_size);
    let (_used_id, _used_len) = harness.wait_for_used().await;
    let out_header = harness.read_out_header(resp_gpa);
    // EROFS = -30
    assert_eq!(
        out_header.error, -30,
        "MKDIR on synthetic root should return EROFS"
    );
}

/// Cross-volume rename: rename a file from inside child A's volume to
/// inside child B's volume must return EXDEV.
#[async_test]
async fn aggregate_rename_across_children_returns_exdev(driver: DefaultDriver) {
    let mut harness = AggregateTestHarness::new(&driver, &["alpha", "beta"]);
    harness.enable().await;
    harness.fuse_init(0).await;

    // Look up both child submount roots so we have FUSE node IDs for
    // each. (Order matters: each lookup posts at a different head_desc.)
    let alpha = harness.lookup_child(2, FUSE_ROOT_ID, b"alpha").await;
    let beta = harness.lookup_child(4, FUSE_ROOT_ID, b"beta").await;
    assert_ne!(alpha.nodeid, beta.nodeid);

    // Issue FUSE_RENAME with olddir = alpha.nodeid, newdir = beta.nodeid.
    // The names don't need to exist — `Real::rename` checks the volume IDs
    // first and short-circuits with EXDEV before any host I/O.
    let rename_args = fuse_rename_in {
        newdir: beta.nodeid,
    };
    let mut args = rename_args.as_bytes().to_vec();
    args.extend_from_slice(b"src\0");
    args.extend_from_slice(b"dst\0");

    let resp_size = OUT_HEADER_SIZE;
    let (_unique, resp_gpa) =
        harness.post_fuse_request(6, FUSE_RENAME, alpha.nodeid, &args, resp_size);
    let (_used_id, _used_len) = harness.wait_for_used().await;
    let out_header = harness.read_out_header(resp_gpa);
    // EXDEV = -18
    assert_eq!(
        out_header.error, -18,
        "rename across aggregate children should return EXDEV"
    );
}

/// `VirtioFs::new_aggregate` rejects an empty children list.
#[test]
fn aggregate_constructor_rejects_empty_children() {
    let err = VirtioFs::new_aggregate(Vec::new()).err();
    assert!(matches!(err, Some(e) if e.value() == lx::Error::EINVAL.value()));
}

/// `VirtioFs::new_aggregate` rejects duplicate child names.
#[test]
fn aggregate_constructor_rejects_duplicate_names() {
    let tmp = tempfile::tempdir().unwrap();
    let children = vec![
        crate::VirtioFsChild {
            name: "shared".into(),
            root_path: tmp.path().to_path_buf(),
            options: None,
        },
        crate::VirtioFsChild {
            name: "shared".into(),
            root_path: tmp.path().to_path_buf(),
            options: None,
        },
    ];
    let err = VirtioFs::new_aggregate(children).err();
    assert!(err.is_some(), "duplicate names must be rejected");
}

/// `VirtioFs::new_aggregate` rejects names containing `/` or NUL, and
/// `.` / `..`.
#[test]
fn aggregate_constructor_rejects_invalid_names() {
    let tmp = tempfile::tempdir().unwrap();
    for bad in ["", ".", "..", "a/b", "with\0nul"] {
        let children = vec![crate::VirtioFsChild {
            name: bad.into(),
            root_path: tmp.path().to_path_buf(),
            options: None,
        }];
        let err = VirtioFs::new_aggregate(children).err();
        assert!(err.is_some(), "name {bad:?} must be rejected");
    }
}

// -------------------------------------------------------------------
// Live aggregate extension (`VirtiofsAggregateHandle::add_child`).
// -------------------------------------------------------------------

/// After `add_child`, the new entry appears in readdir of the synthetic
/// root and is lookup-able by name with FUSE_ATTR_SUBMOUNT set.
#[async_test]
async fn aggregate_live_extension_visible_to_lookup(driver: DefaultDriver) {
    let mut harness = AggregateTestHarness::new(&driver, &["alpha"]);
    harness.enable().await;
    harness.fuse_init(0).await;

    // Sanity: looking up "beta" before extension returns ENOENT.
    let name = b"beta\0";
    let resp_size = OUT_HEADER_SIZE + size_of::<fuse_entry_out>() as u32;
    let (_unique, resp_gpa) =
        harness.post_fuse_request(2, FUSE_LOOKUP, FUSE_ROOT_ID, name, resp_size);
    let (_used_id, _used_len) = harness.wait_for_used().await;
    let out_header = harness.read_out_header(resp_gpa);
    assert_eq!(
        out_header.error, -2,
        "LOOKUP before add_child should be ENOENT"
    );

    // Append a new child.
    let path = harness.alloc_child_tmpdir();
    harness
        .aggregate_handle
        .add_child(crate::VirtioFsChild {
            name: "beta".into(),
            root_path: path,
            options: None,
        })
        .expect("add_child should succeed");

    // GETATTR on root now reports nlink = 2 + 2 = 4.
    let getattr_args = fuse_getattr_in {
        getattr_flags: 0,
        dummy: 0,
        fh: 0,
    };
    let resp_size = OUT_HEADER_SIZE + size_of::<fuse_attr_out>() as u32;
    let (_unique, resp_gpa) = harness.post_fuse_request(
        4,
        FUSE_GETATTR,
        FUSE_ROOT_ID,
        getattr_args.as_bytes(),
        resp_size,
    );
    let (_used_id, _used_len) = harness.wait_for_used().await;
    let out_header = harness.read_out_header(resp_gpa);
    assert_eq!(out_header.error, 0);
    let attr_out: fuse_attr_out = harness.read_response(resp_gpa);
    assert_eq!(
        attr_out.attr.nlink, 4,
        "nlink should grow to 2 + 2 after add_child"
    );

    // LOOKUP of new child succeeds and carries FUSE_ATTR_SUBMOUNT.
    let entry = harness.lookup_child(6, FUSE_ROOT_ID, b"beta").await;
    assert_ne!(entry.nodeid, 0);
    assert_ne!(
        entry.attr.flags & FUSE_ATTR_SUBMOUNT,
        0,
        "dynamically-added child must also carry FUSE_ATTR_SUBMOUNT"
    );
}

/// `add_child` rejects a duplicate name with EEXIST. The original child
/// is not disturbed.
#[test]
fn aggregate_add_child_duplicate_name_returns_eexist() {
    let tmp1 = tempfile::tempdir().unwrap();
    let tmp2 = tempfile::tempdir().unwrap();
    let children = vec![crate::VirtioFsChild {
        name: "shared".into(),
        root_path: tmp1.path().to_path_buf(),
        options: None,
    }];
    let fs = VirtioFs::new_aggregate(children).unwrap();
    let handle = fs.aggregate_handle().unwrap();
    let err = handle
        .add_child(crate::VirtioFsChild {
            name: "shared".into(),
            root_path: tmp2.path().to_path_buf(),
            options: None,
        })
        .expect_err("duplicate add_child must fail");
    assert_eq!(err.value(), lx::Error::EEXIST.value());
}

/// `add_child` rejects an invalid name with EINVAL (delegating to
/// `validate_child_name`).
#[test]
fn aggregate_add_child_invalid_name_returns_einval() {
    let tmp = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let children = vec![crate::VirtioFsChild {
        name: "alpha".into(),
        root_path: tmp.path().to_path_buf(),
        options: None,
    }];
    let fs = VirtioFs::new_aggregate(children).unwrap();
    let handle = fs.aggregate_handle().unwrap();
    for bad in ["", ".", "..", "a/b", "with\0nul"] {
        let err = handle
            .add_child(crate::VirtioFsChild {
                name: bad.into(),
                root_path: other.path().to_path_buf(),
                options: None,
            })
            .err()
            .unwrap_or_else(|| panic!("name {bad:?} should be rejected"));
        assert_eq!(err.value(), lx::Error::EINVAL.value(), "name = {bad:?}");
    }
}

/// `add_child` rejects a child whose readonly setting differs from the
/// aggregate's. The initial aggregate here is read-write (default), so
/// adding a read-only child must fail with EINVAL.
#[test]
fn aggregate_add_child_readonly_mismatch_returns_einval() {
    let tmp = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let children = vec![crate::VirtioFsChild {
        name: "rw".into(),
        root_path: tmp.path().to_path_buf(),
        options: None,
    }];
    let fs = VirtioFs::new_aggregate(children).unwrap();
    let handle = fs.aggregate_handle().unwrap();
    let ro_opts = LxVolumeOptions::from_option_string("ro");
    let err = handle
        .add_child(crate::VirtioFsChild {
            name: "ro".into(),
            root_path: other.path().to_path_buf(),
            options: Some(ro_opts),
        })
        .expect_err("readonly mismatch must fail");
    assert_eq!(err.value(), lx::Error::EINVAL.value());
}

/// After the owning `VirtioFs` drops, calls on a retained handle must
/// return EAGAIN (the aggregate is `TearingDown`).
#[test]
fn aggregate_add_child_after_drop_returns_eagain() {
    let tmp = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let children = vec![crate::VirtioFsChild {
        name: "alpha".into(),
        root_path: tmp.path().to_path_buf(),
        options: None,
    }];
    let fs = VirtioFs::new_aggregate(children).unwrap();
    let handle = fs.aggregate_handle().unwrap();
    drop(fs);
    let err = handle
        .add_child(crate::VirtioFsChild {
            name: "beta".into(),
            root_path: other.path().to_path_buf(),
            options: None,
        })
        .expect_err("add_child after device drop must fail");
    assert_eq!(err.value(), lx::Error::EAGAIN.value());
}
