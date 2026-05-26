// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the virtio-9p device.
//!
//! These tests construct a full `VirtioPlan9Device` backed by a real temp
//! directory, then drive 9P2000.L requests through the descriptor ring just
//! as a guest kernel would.

use crate::VirtioPlan9Device;
use guestmem::GuestMemory;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_event::Event;
use plan9::Plan9FileSystem;
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

// --- 9P2000.L protocol constants (not exported by the plan9 crate) ---

const HEADER_SIZE: usize = 7; // u32 size + u8 type + u16 tag

// Message types
const TVERSION: u8 = 100;
const RVERSION: u8 = 101;
const TATTACH: u8 = 104;
const RATTACH: u8 = 105;
const RLERROR: u8 = 7;
const TWALK: u8 = 110;
const RWALK: u8 = 111;
const TGETATTR: u8 = 24;
const RGETATTR: u8 = 25;

const PROTOCOL_VERSION: &[u8] = b"9P2000.L";

// QID is 13 bytes: u8 type + u32 version + u64 path
const QID_TYPE_DIRECTORY: u8 = 0x80;

// --- Virtqueue constants ---

const QUEUE_SIZE: u16 = 16;

const DESC_ADDR: u64 = 0x0000;
const AVAIL_ADDR: u64 = 0x1000;
const USED_ADDR: u64 = 0x2000;

const DATA_BASE: u64 = 0x10000;
const TOTAL_MEM_SIZE: usize = 0x40000;

// --- 9P message encoding helpers ---

/// A simple 9P message builder. Messages are laid out as:
///   [u32 total_size] [u8 type] [u16 tag] [payload...]
/// Total size includes the 7-byte header itself.
struct P9MessageBuilder {
    buf: Vec<u8>,
}

impl P9MessageBuilder {
    fn new(msg_type: u8, tag: u16) -> Self {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u32.to_le_bytes()); // placeholder for size
        buf.push(msg_type);
        buf.extend_from_slice(&tag.to_le_bytes());
        Self { buf }
    }

    fn u32(mut self, val: u32) -> Self {
        self.buf.extend_from_slice(&val.to_le_bytes());
        self
    }

    fn u64(mut self, val: u64) -> Self {
        self.buf.extend_from_slice(&val.to_le_bytes());
        self
    }

    fn string(mut self, s: &str) -> Self {
        self.buf.extend_from_slice(&(s.len() as u16).to_le_bytes());
        self.buf.extend_from_slice(s.as_bytes());
        self
    }

    /// Write a name sequence: u16 count followed by count names.
    fn name_seq(mut self, names: &[&str]) -> Self {
        self.buf
            .extend_from_slice(&(names.len() as u16).to_le_bytes());
        for name in names {
            self.buf
                .extend_from_slice(&(name.len() as u16).to_le_bytes());
            self.buf.extend_from_slice(name.as_bytes());
        }
        self
    }

    fn build(mut self) -> Vec<u8> {
        let size = self.buf.len() as u32;
        self.buf[..4].copy_from_slice(&size.to_le_bytes());
        self.buf
    }
}

/// Parse a 9P response header. Returns (type, tag).
fn parse_response_header(data: &[u8]) -> (u8, u16) {
    assert!(data.len() >= HEADER_SIZE);
    let msg_type = data[4];
    let tag = u16::from_le_bytes([data[5], data[6]]);
    (msg_type, tag)
}

/// Read a u32 at the given offset in a response (after the 7-byte header).
fn read_resp_u32(data: &[u8], payload_offset: usize) -> u32 {
    let off = HEADER_SIZE + payload_offset;
    u32::from_le_bytes(data[off..off + 4].try_into().unwrap())
}

/// Read a QID type byte at the given payload offset.
fn read_resp_qid_type(data: &[u8], payload_offset: usize) -> u8 {
    data[HEADER_SIZE + payload_offset]
}

/// Read a protocol string at the given payload offset. Returns the string.
fn read_resp_string(data: &[u8], payload_offset: usize) -> String {
    let off = HEADER_SIZE + payload_offset;
    let len = u16::from_le_bytes(data[off..off + 2].try_into().unwrap()) as usize;
    String::from_utf8(data[off + 2..off + 2 + len].to_vec()).unwrap()
}

// --- Test Harness ---

struct TestHarness {
    device: VirtioPlan9Device,
    mem: GuestMemory,
    driver: DefaultDriver,
    queue_event: Event,
    interrupt_event: Event,
    avail_idx: u16,
    used_idx: u16,
    next_data_offset: u64,
    next_tag: u16,
    _tmpdir: tempfile::TempDir,
}

impl TestHarness {
    fn new(driver: &DefaultDriver) -> Self {
        let tmpdir = tempfile::tempdir().unwrap();

        let mem = GuestMemory::allocate(TOTAL_MEM_SIZE);
        init_avail_ring(&mem, AVAIL_ADDR);
        init_used_ring(&mem, USED_ADDR);

        let fs = Plan9FileSystem::new(tmpdir.path(), false).unwrap();
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
        let device = VirtioPlan9Device::new(&driver_source, "test9p", fs);

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
            next_tag: 0,
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

    fn next_tag(&mut self) -> u16 {
        let t = self.next_tag;
        self.next_tag += 1;
        t
    }

    /// Post a 9P request and wait for the response.
    /// Returns the raw response bytes (including the 7-byte header).
    async fn transact(&mut self, head_desc: u16, request: &[u8], resp_buf_size: u32) -> Vec<u8> {
        let req_gpa = self.alloc_data(request.len() as u32);
        let resp_gpa = self.alloc_data(resp_buf_size);

        self.mem.write_at(req_gpa, request).unwrap();
        self.mem
            .write_at(resp_gpa, &vec![0u8; resp_buf_size as usize])
            .unwrap();

        // desc 0: request (readable)
        let flags0 = DescriptorFlags::new().with_next(true);
        write_descriptor(
            &self.mem,
            DESC_ADDR,
            head_desc,
            req_gpa,
            request.len() as u32,
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
            resp_buf_size,
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

        let (used_id, used_len) = wait_for_used(
            &self.driver,
            &self.interrupt_event,
            &self.mem,
            USED_ADDR,
            QUEUE_SIZE,
            &mut self.used_idx,
        )
        .await;

        assert_eq!(
            used_id, head_desc,
            "used ring entry should reference the head descriptor"
        );
        assert!(
            used_len <= resp_buf_size,
            "used_len ({used_len}) exceeds response buffer size ({resp_buf_size})"
        );

        let mut resp = vec![0u8; used_len as usize];
        self.mem.read_at(resp_gpa, &mut resp).unwrap();
        resp
    }

    /// Send Tversion and verify the Rversion response.
    async fn version(&mut self, head_desc: u16) {
        let tag = self.next_tag();
        let msg = P9MessageBuilder::new(TVERSION, tag)
            .u32(8192) // msize
            .string(std::str::from_utf8(PROTOCOL_VERSION).unwrap())
            .build();

        let resp = self.transact(head_desc, &msg, 256).await;
        let (rtype, rtag) = parse_response_header(&resp);
        assert_eq!(rtype, RVERSION, "expected Rversion");
        assert_eq!(rtag, tag);

        let version = read_resp_string(&resp, 4); // skip msize (4 bytes)
        assert_eq!(version, "9P2000.L");
    }

    /// Send Tattach and verify the Rattach response. Returns the root qid type.
    async fn attach(&mut self, head_desc: u16, fid: u32) -> u8 {
        let tag = self.next_tag();
        let msg = P9MessageBuilder::new(TATTACH, tag)
            .u32(fid) // fid
            .u32(u32::MAX) // afid (NOFID)
            .string("root") // uname
            .string("") // aname
            .u32(0) // n_uname
            .build();

        let resp = self.transact(head_desc, &msg, 256).await;
        let (rtype, rtag) = parse_response_header(&resp);
        assert_eq!(rtype, RATTACH, "expected Rattach");
        assert_eq!(rtag, tag);

        // Return the qid type byte
        read_resp_qid_type(&resp, 0)
    }

    fn tmpdir_path(&self) -> &std::path::Path {
        self._tmpdir.path()
    }
}

// --- Tests ---

/// Tversion handshake succeeds and negotiates 9P2000.L.
#[async_test]
async fn version_succeeds(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;
    harness.version(0).await;
}

/// Tattach after Tversion returns a directory qid for the root.
#[async_test]
async fn attach_returns_directory(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;
    harness.version(0).await;

    let qid_type = harness.attach(2, 1).await;
    assert_eq!(
        qid_type, QID_TYPE_DIRECTORY,
        "root qid should be a directory"
    );
}

/// Twalk to an existing file succeeds and returns a qid.
#[async_test]
async fn walk_existing_file(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    std::fs::write(harness.tmpdir_path().join("hello.txt"), "test data").unwrap();

    harness.enable().await;
    harness.version(0).await;
    harness.attach(2, 1).await;

    let tag = harness.next_tag();
    let msg = P9MessageBuilder::new(TWALK, tag)
        .u32(1) // fid (root, from attach)
        .u32(2) // newfid
        .name_seq(&["hello.txt"])
        .build();

    let resp = harness.transact(4, &msg, 256).await;
    let (rtype, rtag) = parse_response_header(&resp);
    assert_eq!(rtype, RWALK, "expected Rwalk");
    assert_eq!(rtag, tag);

    // Rwalk payload: u16 count + count * qid
    let count = u16::from_le_bytes(resp[HEADER_SIZE..HEADER_SIZE + 2].try_into().unwrap());
    assert_eq!(count, 1, "should have walked 1 path component");
}

/// Twalk to a non-existent file returns Rlerror.
#[async_test]
async fn walk_nonexistent_returns_error(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;
    harness.version(0).await;
    harness.attach(2, 1).await;

    let tag = harness.next_tag();
    let msg = P9MessageBuilder::new(TWALK, tag)
        .u32(1) // fid (root)
        .u32(2) // newfid
        .name_seq(&["does_not_exist.txt"])
        .build();

    let resp = harness.transact(4, &msg, 256).await;
    let (rtype, rtag) = parse_response_header(&resp);
    assert_eq!(rtype, RLERROR, "expected Rlerror for missing file");
    assert_eq!(rtag, tag);
}

/// Tgetattr on the root fid returns directory attributes.
#[async_test]
async fn getattr_root(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;
    harness.version(0).await;
    harness.attach(2, 1).await;

    let tag = harness.next_tag();
    // request_mask: P9_GETATTR_BASIC = 0x000007ff requests all basic fields
    let msg = P9MessageBuilder::new(TGETATTR, tag)
        .u32(1) // fid (root)
        .u64(0x000007ff) // request_mask
        .build();

    let resp = harness.transact(4, &msg, 512).await;
    let (rtype, rtag) = parse_response_header(&resp);
    assert_eq!(rtype, RGETATTR, "expected Rgetattr");
    assert_eq!(rtag, tag);

    // Rgetattr payload: u64 valid + qid(13 bytes) + u32 mode + ...
    // qid starts at offset 8 (after u64 valid)
    let qid_type = read_resp_qid_type(&resp, 8);
    assert_eq!(qid_type, QID_TYPE_DIRECTORY, "root should be a directory");

    // mode is at offset 8 + 13 = 21
    let mode = read_resp_u32(&resp, 21);
    assert_eq!(mode & 0o170000, 0o040000, "mode should indicate directory");
}

/// A malformed message (too short to parse) should still complete the
/// descriptor rather than hanging the queue.
#[async_test]
async fn malformed_request_completes(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;

    // Post garbage that's too short to be a valid 9P header
    let garbage = [0xFF; 4];
    let req_gpa = harness.alloc_data(garbage.len() as u32);
    harness.mem.write_at(req_gpa, &garbage).unwrap();

    let resp_size = 256u32;
    let resp_gpa = harness.alloc_data(resp_size);
    harness
        .mem
        .write_at(resp_gpa, &vec![0u8; resp_size as usize])
        .unwrap();

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

    // Should complete (possibly with 0 bytes) rather than hang.
    let (_used_id, _used_len) = wait_for_used(
        &harness.driver,
        &harness.interrupt_event,
        &harness.mem,
        USED_ADDR,
        QUEUE_SIZE,
        &mut harness.used_idx,
    )
    .await;
}
