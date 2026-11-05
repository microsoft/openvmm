// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the virtio-fs device.
//!
//! These tests construct a full `VirtioFsDevice` backed by a real temp
//! directory, then drive FUSE requests through the descriptor ring just
//! as a guest kernel would.

use crate::VirtioFs;
use crate::virtio::VirtioFsDevice;
use crate::virtio::VirtioFsDeviceOptions;
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
use zerocopy::FromZeros;
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
        let device = VirtioFsDevice::new(
            &driver_source,
            "testfs",
            fs,
            VirtioFsDeviceOptions::default(),
        );

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

#[test]
fn child_operations_require_directory_parents() {
    for aggregate in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file"), b"data").unwrap();
        let (fs, root_id) = if aggregate {
            let fs = VirtioFs::new_aggregate();
            fs.add_child("share", tmp.path(), None).unwrap();
            let root = fs.lookup_synthetic_root("share".as_ref()).unwrap();
            (fs, root.nodeid)
        } else {
            (VirtioFs::new(tmp.path(), None).unwrap(), FUSE_ROOT_ID)
        };
        let root = fs.get_inode(root_id).unwrap();
        let (file, _) = root.lookup_child("file".as_ref()).unwrap();
        let (symlink, _) = root
            .symlink("link".as_ref(), "file".as_ref(), 0, 0)
            .unwrap();
        for parent in [&file, &symlink] {
            let name = "child".as_ref();
            assert_eq!(parent.lookup_child(name).err().unwrap(), lx::Error::ENOTDIR);
            assert_eq!(
                parent
                    .create(name, lx::O_WRONLY, 0o666, 0, 0)
                    .err()
                    .unwrap(),
                lx::Error::ENOTDIR
            );
            assert_eq!(
                parent.mkdir(name, 0o777, 0, 0).err().unwrap(),
                lx::Error::ENOTDIR
            );
            assert_eq!(
                parent
                    .mknod(name, lx::S_IFREG | 0o666, 0, 0, 0)
                    .err()
                    .unwrap(),
                lx::Error::ENOTDIR
            );
            assert_eq!(
                parent.symlink(name, "file".as_ref(), 0, 0).err().unwrap(),
                lx::Error::ENOTDIR
            );
            assert_eq!(parent.link(name, &file).unwrap_err(), lx::Error::ENOTDIR);
            assert_eq!(parent.unlink(name, 0).unwrap_err(), lx::Error::ENOTDIR);
            assert_eq!(
                parent.rename(name, &root, name, 0).unwrap_err(),
                lx::Error::ENOTDIR
            );
            assert_eq!(
                root.rename("file".as_ref(), parent, name, 0).unwrap_err(),
                lx::Error::ENOTDIR
            );
        }
        assert_eq!(std::fs::read(tmp.path().join("file")).unwrap(), b"data");
        assert!(!tmp.path().join("child").exists());
    }
}

#[async_test]
async fn fuse_create_rejects_symlink_parent(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;
    harness.fuse_init(0).await;
    let (_, response) = harness.post_fuse_request(
        2,
        FUSE_SYMLINK,
        FUSE_ROOT_ID,
        b"link\0missing\0",
        OUT_HEADER_SIZE + size_of::<fuse_entry_out>() as u32,
    );
    harness.wait_for_used().await;
    assert_eq!(harness.read_out_header(response).error, 0);
    let entry: fuse_entry_out = harness.read_response(response);
    let mut create = fuse_create_in::new_zeroed();
    create.flags = (lx::O_WRONLY | lx::O_TRUNC) as u32;
    create.mode = 0o666;
    let mut args = create.as_bytes().to_vec();
    args.extend_from_slice(b"child\0");
    let (unique, response) = harness.post_fuse_request(
        4,
        FUSE_CREATE,
        entry.nodeid,
        &args,
        OUT_HEADER_SIZE + size_of::<fuse_entry_out>() as u32 + size_of::<fuse_open_out>() as u32,
    );
    let (_, used_len) = harness.wait_for_used().await;
    assert_eq!(used_len, OUT_HEADER_SIZE);
    let header = harness.read_out_header(response);
    assert_eq!(header.unique, unique);
    assert_eq!(header.error, -lx::ENOTDIR);
    assert!(!harness.tmpdir_path().join("child").exists());
}

#[test]
fn final_symlinks_are_not_opened_or_created() {
    for aggregate in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("target"), b"unchanged").unwrap();
        std::fs::write(tmp.path().join("stale"), b"original").unwrap();
        let (fs, root_id) = if aggregate {
            let fs = VirtioFs::new_aggregate();
            fs.add_child("share", tmp.path(), None).unwrap();
            let root = fs.lookup_synthetic_root("share".as_ref()).unwrap();
            (fs, root.nodeid)
        } else {
            (VirtioFs::new(tmp.path(), None).unwrap(), FUSE_ROOT_ID)
        };
        let root = fs.get_inode(root_id).unwrap();
        let (stale, _) = root.lookup_child("stale".as_ref()).unwrap();
        std::fs::rename(tmp.path().join("stale"), tmp.path().join("original")).unwrap();
        root.symlink("stale".as_ref(), "target".as_ref(), 0, 0)
            .unwrap();
        let (link, _) = root
            .symlink("link".as_ref(), "target".as_ref(), 0, 0)
            .unwrap();
        for inode in [std::sync::Arc::new(link), std::sync::Arc::new(stale)] {
            for flags in [
                lx::O_RDONLY,
                lx::O_WRONLY,
                lx::O_RDWR,
                lx::O_WRONLY | lx::O_TRUNC,
            ] {
                assert_eq!(inode.clone().open(flags).err().unwrap(), lx::Error::ELOOP);
            }
        }
        for name in ["link", "stale"] {
            for flags in [
                lx::O_RDONLY,
                lx::O_WRONLY,
                lx::O_RDWR,
                lx::O_WRONLY | lx::O_TRUNC,
            ] {
                assert_eq!(
                    root.create(name.as_ref(), flags, 0o666, 0, 0)
                        .err()
                        .unwrap(),
                    lx::Error::ELOOP
                );
            }
            let (link, _) = root.lookup_child(name.as_ref()).unwrap();
            assert_eq!(link.read_link().unwrap(), "target");
        }
        let (file, _) = root.lookup_child("target".as_ref()).unwrap();
        let file = std::sync::Arc::new(file);
        file.open(lx::O_RDONLY).unwrap();
        root.create("new".as_ref(), lx::O_WRONLY, 0o666, 0, 0)
            .unwrap();
        assert_eq!(
            std::fs::read(tmp.path().join("target")).unwrap(),
            b"unchanged"
        );
        assert_eq!(
            std::fs::read(tmp.path().join("original")).unwrap(),
            b"original"
        );
        assert!(tmp.path().join("new").is_file());
    }
}

#[async_test]
async fn fuse_open_and_create_reject_final_symlink(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    std::fs::write(harness.tmpdir_path().join("target"), b"unchanged").unwrap();
    harness.enable().await;
    harness.fuse_init(0).await;
    let (_, response) = harness.post_fuse_request(
        2,
        FUSE_SYMLINK,
        FUSE_ROOT_ID,
        b"link\0target\0",
        OUT_HEADER_SIZE + size_of::<fuse_entry_out>() as u32,
    );
    harness.wait_for_used().await;
    assert_eq!(harness.read_out_header(response).error, 0);
    let entry: fuse_entry_out = harness.read_response(response);

    let open = fuse_open_in::new_zeroed();
    let mut create = fuse_create_in::new_zeroed();
    create.mode = 0o666;
    let mut create_args = create.as_bytes().to_vec();
    create_args.extend_from_slice(b"link\0");
    for (opcode, nodeid, args) in [
        (FUSE_OPEN, entry.nodeid, open.as_bytes()),
        (FUSE_CREATE, FUSE_ROOT_ID, create_args.as_slice()),
    ] {
        let (unique, response) = harness.post_fuse_request(4, opcode, nodeid, args, 256);
        let (_, used_len) = harness.wait_for_used().await;
        assert_eq!(used_len, OUT_HEADER_SIZE);
        let header = harness.read_out_header(response);
        assert_eq!(header.unique, unique);
        assert_eq!(header.error, -lx::ELOOP);
    }
    assert_eq!(
        std::fs::read(harness.tmpdir_path().join("target")).unwrap(),
        b"unchanged"
    );
}

#[async_test]
async fn execute_intent_open_reads_file(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    let contents = b"executable";
    std::fs::write(harness.tmpdir_path().join("file"), contents).unwrap();
    harness.enable().await;
    harness.fuse_init(0).await;

    let (_, response) = harness.post_fuse_request(
        2,
        FUSE_LOOKUP,
        FUSE_ROOT_ID,
        b"file\0",
        OUT_HEADER_SIZE + size_of::<fuse_entry_out>() as u32,
    );
    harness.wait_for_used().await;
    assert_eq!(harness.read_out_header(response).error, 0);
    let entry: fuse_entry_out = harness.read_response(response);

    let mut open = fuse_open_in::new_zeroed();
    open.flags = (lx::O_RDONLY | lx::O_LARGEFILE) as u32 | 0x20;
    let (unique, response) = harness.post_fuse_request(
        4,
        FUSE_OPEN,
        entry.nodeid,
        open.as_bytes(),
        OUT_HEADER_SIZE + size_of::<fuse_open_out>() as u32,
    );
    harness.wait_for_used().await;
    let header = harness.read_out_header(response);
    assert_eq!(header.unique, unique);
    assert_eq!(header.error, 0);
    let opened: fuse_open_out = harness.read_response(response);

    let mut read = fuse_read_in::new_zeroed();
    read.fh = opened.fh;
    read.size = contents.len() as u32;
    let (unique, response) = harness.post_fuse_request(
        6,
        FUSE_READ,
        entry.nodeid,
        read.as_bytes(),
        OUT_HEADER_SIZE + read.size,
    );
    let (_, used_len) = harness.wait_for_used().await;
    assert_eq!(used_len, OUT_HEADER_SIZE + read.size);
    let header = harness.read_out_header(response);
    assert_eq!(header.unique, unique);
    assert_eq!(header.error, 0);
    let mut actual = vec![0; contents.len()];
    harness
        .mem
        .read_at(response + OUT_HEADER_SIZE as u64, &mut actual)
        .unwrap();
    assert_eq!(actual, contents);
}

#[async_test]
async fn unsupported_open_flags_do_not_change_files(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    std::fs::write(harness.tmpdir_path().join("file"), b"unchanged").unwrap();
    harness.enable().await;
    harness.fuse_init(0).await;
    let (_, response) = harness.post_fuse_request(
        2,
        FUSE_LOOKUP,
        FUSE_ROOT_ID,
        b"file\0",
        OUT_HEADER_SIZE + size_of::<fuse_entry_out>() as u32,
    );
    harness.wait_for_used().await;
    assert_eq!(harness.read_out_header(response).error, 0);
    let entry: fuse_entry_out = harness.read_response(response);

    for unsupported in [
        lx::O_NOCTTY as u32,
        lx::O_CLOEXEC as u32,
        lx::O_PATH as u32,
        0x2000,
        0x400000,
        1 << 31,
    ] {
        let flags = unsupported | (lx::O_WRONLY | lx::O_TRUNC) as u32;
        let mut open = fuse_open_in::new_zeroed();
        open.flags = flags;
        let mut create = fuse_create_in::new_zeroed();
        create.flags = flags;
        create.mode = 0o666;
        let mut existing = create.as_bytes().to_vec();
        existing.extend_from_slice(b"file\0");
        let mut new = create.as_bytes().to_vec();
        new.extend_from_slice(b"new\0");
        for (opcode, nodeid, args) in [
            (FUSE_OPEN, entry.nodeid, open.as_bytes()),
            (FUSE_CREATE, FUSE_ROOT_ID, existing.as_slice()),
            (FUSE_CREATE, FUSE_ROOT_ID, new.as_slice()),
            (FUSE_OPEN, u64::MAX, open.as_bytes()),
            (FUSE_OPENDIR, u64::MAX, open.as_bytes()),
            (FUSE_CREATE, u64::MAX, new.as_slice()),
        ] {
            let (unique, response) = harness.post_fuse_request(4, opcode, nodeid, args, 256);
            let (_, used_len) = harness.wait_for_used().await;
            assert_eq!(used_len, OUT_HEADER_SIZE);
            let header = harness.read_out_header(response);
            assert_eq!(header.unique, unique);
            assert_eq!(header.error, -lx::EINVAL);
        }
    }
    assert_eq!(
        std::fs::read(harness.tmpdir_path().join("file")).unwrap(),
        b"unchanged"
    );
    assert!(!harness.tmpdir_path().join("new").exists());
}

#[test]
fn synthetic_root_open_dir_rejects_unsupported_flags() {
    let fs = VirtioFs::new_aggregate();
    let mut header = fuse_in_header::new_zeroed();
    header.len = (size_of::<fuse_in_header>() + size_of::<fuse_open_in>()) as u32;
    header.opcode = FUSE_OPENDIR;
    header.nodeid = FUSE_ROOT_ID;
    for flags in [
        lx::O_CREAT,
        lx::O_EXCL,
        lx::O_NOCTTY,
        lx::O_CLOEXEC,
        lx::O_PATH,
    ] {
        let mut open = fuse_open_in::new_zeroed();
        open.flags = flags as u32;
        let mut data = header.as_bytes().to_vec();
        data.extend_from_slice(open.as_bytes());
        let request = fuse::Request::new(data.as_slice()).unwrap();

        assert_eq!(
            fuse::Fuse::open_dir(&fs, &request, open.flags).unwrap_err(),
            lx::Error::EINVAL
        );
    }
}

#[test]
#[cfg(unix)]
fn replaced_regular_file_does_not_wait_for_fifo_peer() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("file"), b"original").unwrap();
    let fs = VirtioFs::new(tmp.path(), None).unwrap();
    let root = fs.get_inode(FUSE_ROOT_ID).unwrap();
    let (inode, _) = root.lookup_child("file".as_ref()).unwrap();
    let inode = std::sync::Arc::new(inode);

    std::fs::remove_file(tmp.path().join("file")).unwrap();
    root.volume
        .mknod(
            "file",
            lxutil::LxCreateOptions::new(lx::S_IFIFO | 0o600, 0, 0),
            0,
        )
        .unwrap();

    drop(inode.clone().open(lx::O_RDONLY).unwrap());
    assert_eq!(inode.open(lx::O_WRONLY).err().unwrap(), lx::Error::ENXIO);
    assert_eq!(
        root.create("file".as_ref(), lx::O_WRONLY, 0o600, 0, 0)
            .err()
            .unwrap(),
        lx::Error::ENXIO
    );
}

#[test]
#[cfg_attr(
    windows,
    ignore = "requires permission to create native Windows symlinks"
)]
fn stale_directory_paths_do_not_follow_native_links() {
    for aggregate in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("parent")).unwrap();
        std::fs::write(tmp.path().join("parent").join("file"), b"inside").unwrap();
        std::fs::write(outside.path().join("file"), b"outside").unwrap();
        let (fs, root_id) = if aggregate {
            let fs = VirtioFs::new_aggregate();
            fs.add_child("share", tmp.path(), None).unwrap();
            let root = fs.lookup_synthetic_root("share".as_ref()).unwrap();
            (fs, root.nodeid)
        } else {
            (VirtioFs::new(tmp.path(), None).unwrap(), FUSE_ROOT_ID)
        };
        let root = fs.get_inode(root_id).unwrap();
        let (parent, _) = root.lookup_child("parent".as_ref()).unwrap();
        let (file, _) = parent.lookup_child("file".as_ref()).unwrap();
        std::fs::rename(tmp.path().join("parent"), tmp.path().join("old")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(outside.path(), tmp.path().join("parent")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("parent")).unwrap();
        assert_eq!(
            parent
                .create("file".as_ref(), lx::O_WRONLY | lx::O_TRUNC, 0o666, 0, 0)
                .err()
                .unwrap(),
            lx::Error::ELOOP
        );
        assert_eq!(
            std::sync::Arc::new(file).open(lx::O_WRONLY).err().unwrap(),
            lx::Error::ELOOP
        );
        let error = root
            .rename("old".as_ref(), &parent, "renamed".as_ref(), 0)
            .unwrap_err();
        assert!(matches!(error, lx::Error::ENOTDIR | lx::Error::ELOOP));
        assert_eq!(
            std::fs::read(outside.path().join("file")).unwrap(),
            b"outside"
        );
        assert!(!outside.path().join("renamed").exists());
    }
}

/// Tests for serving a file system under a user token on Windows.
#[cfg(windows)]
mod user_token {
    // UNSAFETY: Calling Win32 APIs and constructing owned handles from their
    // outputs.
    #![expect(unsafe_code)]

    use crate::session::VirtioFsSession;
    use fuse::Fuse;
    use fuse::ReplySender;
    use fuse::Request;
    use fuse::SessionInfo;
    use fuse::protocol::*;
    use pal::windows::security::RevertToSelfGuard;
    use pal::windows::security::duplicate_impersonation_token;
    use pal::windows::security::open_process_token;
    use std::io;
    use std::os::windows::io::AsHandle;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::io::FromRawHandle;
    use std::os::windows::io::OwnedHandle;
    use std::ptr;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;
    use windows_sys::Win32::Foundation::ERROR_NO_TOKEN;
    use windows_sys::Win32::Security;
    use windows_sys::Win32::System::Threading::GetCurrentThread;
    use windows_sys::Win32::System::Threading::OpenThreadToken;
    use zerocopy::FromBytes;
    use zerocopy::FromZeros;
    use zerocopy::IntoBytes;

    fn process_token() -> OwnedHandle {
        let token = open_process_token(Security::TOKEN_DUPLICATE).unwrap();
        duplicate_impersonation_token(token.as_handle()).unwrap()
    }

    fn thread_token() -> io::Result<OwnedHandle> {
        let mut token = ptr::null_mut();
        // SAFETY: Calling Win32 API as documented.
        unsafe {
            if OpenThreadToken(GetCurrentThread(), Security::TOKEN_QUERY, 1, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(OwnedHandle::from_raw_handle(token))
        }
    }

    #[test]
    fn rejects_non_token_handle() {
        let file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        duplicate_impersonation_token(file.as_handle()).unwrap_err();
    }

    #[test]
    fn impersonates_until_dropped() {
        let process_token = process_token();
        let token = duplicate_impersonation_token(process_token.as_handle()).unwrap();
        {
            let _guard = RevertToSelfGuard::impersonate(&token).unwrap();
            thread_token().unwrap();
        }

        let err = thread_token().unwrap_err();
        assert_eq!(err.raw_os_error(), Some(ERROR_NO_TOKEN as i32));
    }

    fn restricted_token() -> OwnedHandle {
        let token = process_token();
        let mut world_sid = [0u32; Security::SECURITY_MAX_SID_SIZE as usize / size_of::<u32>()];
        let mut sid_size = size_of_val(&world_sid) as u32;
        let mut restricted = ptr::null_mut();
        // SAFETY: The SID buffer is aligned and large enough for a well-known
        // SID. All inputs remain valid until the new token handle is returned.
        unsafe {
            assert_ne!(
                Security::CreateWellKnownSid(
                    Security::WinWorldSid,
                    ptr::null_mut(),
                    world_sid.as_mut_ptr().cast(),
                    &mut sid_size,
                ),
                0
            );
            let sid = Security::SID_AND_ATTRIBUTES {
                Sid: world_sid.as_mut_ptr().cast(),
                Attributes: 0,
            };
            assert_ne!(
                Security::CreateRestrictedToken(
                    token.as_raw_handle(),
                    Security::DISABLE_MAX_PRIVILEGE,
                    0,
                    ptr::null(),
                    0,
                    ptr::null(),
                    1,
                    &sid,
                    &mut restricted,
                ),
                0
            );
            OwnedHandle::from_raw_handle(restricted)
        }
    }

    fn token_id(token: &OwnedHandle) -> (u32, i32) {
        let mut stats = Security::TOKEN_STATISTICS::default();
        let mut size = 0;
        // SAFETY: The token is valid and the output buffer matches the queried
        // information class.
        unsafe {
            assert_ne!(
                Security::GetTokenInformation(
                    token.as_raw_handle(),
                    Security::TokenStatistics,
                    ptr::from_mut(&mut stats).cast(),
                    size_of_val(&stats) as u32,
                    &mut size,
                ),
                0
            );
        }
        (stats.TokenId.LowPart, stats.TokenId.HighPart)
    }

    struct TokenCheckingFs {
        token_id: (u32, i32),
        calls: Arc<AtomicUsize>,
    }

    impl TokenCheckingFs {
        fn check_identity(&self) {
            let token = thread_token().unwrap();
            assert_eq!(token_id(&token), self.token_id);
            // SAFETY: The handle refers to a valid token.
            let restricted = unsafe { Security::IsTokenRestricted(token.as_raw_handle()) };
            assert_ne!(restricted, 0);
            self.calls.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl Fuse for TokenCheckingFs {
        fn init(&self, _info: &mut SessionInfo) {
            self.check_identity();
        }

        fn get_attr(&self, _request: &Request, _flags: u32, _fh: u64) -> lx::Result<fuse_attr_out> {
            self.check_identity();
            Err(lx::Error::EACCES)
        }
    }

    #[derive(Default)]
    struct TestSender(Vec<u8>);

    impl ReplySender for TestSender {
        fn send(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
            self.0 = bufs.iter().flat_map(|buf| buf.iter().copied()).collect();
            Ok(())
        }
    }

    #[test]
    fn session_dispatch_impersonates_restricted_token_and_reverts() {
        assert_eq!(
            thread_token().unwrap_err().raw_os_error(),
            Some(ERROR_NO_TOKEN as i32)
        );
        let token = restricted_token();
        let calls = Arc::new(AtomicUsize::new(0));
        let session = VirtioFsSession::new(TokenCheckingFs {
            token_id: token_id(&token),
            calls: calls.clone(),
        })
        .with_user_token(Some(token));
        let init = fuse_init_in {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            ..FromZeros::new_zeroed()
        };
        let getattr = fuse_getattr_in::new_zeroed();
        for (opcode, args, expected_error) in [
            (FUSE_INIT, init.as_bytes(), 0),
            (FUSE_GETATTR, getattr.as_bytes(), -lx::Error::EACCES.value()),
        ] {
            let header = fuse_in_header {
                len: (size_of::<fuse_in_header>() + args.len()) as u32,
                opcode,
                unique: 1,
                nodeid: 1,
                ..FromZeros::new_zeroed()
            };
            let mut request = header.as_bytes().to_vec();
            request.extend_from_slice(args);
            let mut sender = TestSender::default();
            session.dispatch(Request::new(request.as_slice()).unwrap(), &mut sender, None);
            let (reply, _) = fuse_out_header::read_from_prefix(&sender.0).unwrap();
            assert_eq!(reply.error, expected_error);
            assert_eq!(
                thread_token().unwrap_err().raw_os_error(),
                Some(ERROR_NO_TOKEN as i32)
            );
        }
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }
}
