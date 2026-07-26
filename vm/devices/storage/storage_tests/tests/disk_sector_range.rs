// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Sector-range validation tests for disk backends.
//!
//! Storage frontends pass guest-supplied sector numbers through to disk
//! backends, so every backend must reject out-of-range and non-representable
//! requests rather than panicking or silently operating on the wrong data.
//!
//! These tests are only meaningful with overflow checks enabled, which is the
//! default for the `dev` profile used by `cargo test` and `cargo nextest`.

use disk_backend::Disk;
use disk_backend::DiskError;
use disk_file::FileDisk;
use disk_striped::StripedDisk;
use guestmem::GuestMemory;
use pal_async::async_test;
use scsi_buffers::OwnedRequestBuffers;
use storage_tests::sector_range::test_disk_representability;
use storage_tests::sector_range::test_disk_sector_range_conformance;
use test_with_tracing::test;

const SECTOR_SIZE: u64 = 512;
const DISK_SIZE: u64 = 1024 * 1024;

fn ram_disk() -> Disk {
    disklayer_ram::ram_disk(DISK_SIZE, false).unwrap()
}

fn file_disk() -> Disk {
    let file = tempfile::tempfile().unwrap();
    file.set_len(DISK_SIZE).unwrap();
    Disk::new(FileDisk::open(file, false).unwrap()).unwrap()
}

fn striped_disk() -> Disk {
    let devices = (0..2).map(|_| ram_disk()).collect();
    Disk::new(StripedDisk::new(devices, None, None).unwrap()).unwrap()
}

#[async_test]
async fn ram_disk_conformance() {
    test_disk_sector_range_conformance(&ram_disk()).await;
}

#[async_test]
async fn file_disk_conformance() {
    test_disk_sector_range_conformance(&file_disk()).await;
}

#[async_test]
async fn striped_disk_conformance() {
    test_disk_sector_range_conformance(&striped_disk()).await;
}

/// Regression test for an arithmetic overflow panic in
/// `disk_layered::bitmap::SectorBitmapRange::end_sector`, which computes
/// `start_sector + bits.len()` without checking for overflow. `LayeredDisk`
/// builds the bitmap directly from the caller-supplied sector, so a read near
/// `u64::MAX` panics in builds with overflow checks enabled.
#[async_test]
async fn layered_disk_read_at_max_sector_does_not_panic() {
    let disk = ram_disk();
    let mem = GuestMemory::allocate(SECTOR_SIZE as usize);
    let r = disk
        .read_vectored(
            &OwnedRequestBuffers::linear(0, SECTOR_SIZE as usize, true).buffer(&mem),
            u64::MAX,
        )
        .await;
    assert!(matches!(r, Err(DiskError::IllegalBlock)), "{r:?}");
}

/// `FileDisk` range checks the request in byte units, computing
/// `sector << sector_shift`. A left shift discards high bits without
/// panicking, so a sufficiently large sector wraps around to a small byte
/// offset and passes the check.
///
/// This defect is the only one in this file whose symptom is silent wrong data
/// rather than a panic, so the test asserts both that the request is rejected
/// and that it did not return the contents of sector 0.
#[async_test]
async fn file_disk_sector_does_not_wrap_when_shifted() {
    let disk = file_disk();
    let mem = GuestMemory::allocate(SECTOR_SIZE as usize);

    // Fill sector 0 with a recognizable pattern.
    mem.write_at(0, &[0xcd; SECTOR_SIZE as usize]).unwrap();
    disk.write_vectored(
        &OwnedRequestBuffers::linear(0, SECTOR_SIZE as usize, false).buffer(&mem),
        0,
        false,
    )
    .await
    .unwrap();
    mem.write_at(0, &[0; SECTOR_SIZE as usize]).unwrap();

    // `1 << 55` shifted left by 9 (512-byte sectors) is `1 << 64`, which
    // truncates to a byte offset of zero.
    let r = disk
        .read_vectored(
            &OwnedRequestBuffers::linear(0, SECTOR_SIZE as usize, true).buffer(&mem),
            1 << 55,
        )
        .await;

    let mut buf = [0; SECTOR_SIZE as usize];
    mem.read_at(0, &mut buf).unwrap();
    assert_ne!(buf, [0xcd; SECTOR_SIZE as usize], "read returned sector 0");
    assert!(matches!(r, Err(DiskError::IllegalBlock)), "{r:?}");
}

/// `StripedDisk` computes `end_sector = start_sector + (len >> sector_shift)`
/// before handing it to `get_chunk_iter`, which is the function that actually
/// range checks. The addition is unchecked, so a request near `u64::MAX` either
/// panics or wraps to a small in-range `end_sector` that passes the check.
#[async_test]
async fn striped_disk_end_sector_does_not_wrap() {
    let disk = striped_disk();
    let mem = GuestMemory::allocate(2 * SECTOR_SIZE as usize);
    let r = disk
        .read_vectored(
            &OwnedRequestBuffers::linear(0, 2 * SECTOR_SIZE as usize, true).buffer(&mem),
            u64::MAX - 1,
        )
        .await;
    assert!(matches!(r, Err(DiskError::IllegalBlock)), "{r:?}");
}

/// The representability guarantee is not specific to 512-byte sectors.
#[async_test]
async fn ram_disk_4k_representability() {
    let disk = disklayer_ram::ram_disk_with_sector_size(DISK_SIZE, false, 4096).unwrap();
    test_disk_representability(&disk).await;
}
