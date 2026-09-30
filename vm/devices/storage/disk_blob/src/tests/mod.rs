// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for the blob disk backend.

mod http_server;

use crate::BlobDisk;
use crate::blob::file::FileBlob;
use crate::blob::http::HttpBlob;
use disk_backend::Disk;
use disk_backend::DiskError;
use guestmem::GuestMemory;
use http_server::Behavior;
use http_server::TestHttpServer;
use pal_async::async_test;
use scsi_buffers::OwnedRequestBuffers;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;

const SECTOR_SIZE: usize = 512;
const DISK_SIZE: u64 = 1024 * 1024;

/// A file whose length is exactly the disk size, so that the backing object's
/// bounds coincide with the disk's.
fn raw_file() -> std::fs::File {
    let file = tempfile::tempfile().unwrap();
    file.set_len(DISK_SIZE).unwrap();
    file
}

/// A fixed VHD1 image: `DISK_SIZE` bytes of data followed by a 512-byte footer,
/// so the blob is strictly larger than the disk it presents.
fn fixed_vhd1_file() -> std::fs::File {
    let file = raw_file();
    disk_vhd1::Vhd1Disk::make_fixed(&file).unwrap();
    file
}

fn read_all(mut file: std::fs::File) -> Vec<u8> {
    let mut buf = Vec::new();
    file.seek(SeekFrom::Start(0)).unwrap();
    file.read_to_end(&mut buf).unwrap();
    buf
}

#[async_test]
async fn sector_range_conformance() {
    let disk = Disk::new(BlobDisk::new(FileBlob::new(raw_file()).unwrap())).unwrap();
    storage_tests::sector_range::test_disk_sector_range_conformance(&disk).await;
}

#[async_test]
async fn sector_range_conformance_fixed_vhd1() {
    let disk = Disk::new(
        BlobDisk::new_fixed_vhd1(FileBlob::new(fixed_vhd1_file()).unwrap())
            .await
            .unwrap(),
    )
    .unwrap();
    storage_tests::sector_range::test_disk_sector_range_conformance(&disk).await;
}

/// Over HTTP, so that the `Blob` implementation under test is `HttpBlob` rather
/// than `FileBlob`. The two are not interchangeable: a defect in one says
/// nothing about the other.
#[async_test]
async fn sector_range_conformance_http() {
    let server = TestHttpServer::new(vec![0; DISK_SIZE as usize], Behavior::Correct);
    let disk = Disk::new(BlobDisk::new(HttpBlob::new(&server.url()).await.unwrap())).unwrap();
    storage_tests::sector_range::test_disk_sector_range_conformance(&disk).await;
}

#[async_test]
async fn sector_range_conformance_http_fixed_vhd1() {
    let server = TestHttpServer::new(read_all(fixed_vhd1_file()), Behavior::Correct);
    let disk = Disk::new(
        BlobDisk::new_fixed_vhd1(HttpBlob::new(&server.url()).await.unwrap())
            .await
            .unwrap(),
    )
    .unwrap();
    storage_tests::sector_range::test_disk_sector_range_conformance(&disk).await;
}

/// A blob larger than the disk it presents is the case where delegating the
/// range check to the backing object is not sufficient: a read one sector past
/// the end lands in the VHD footer and succeeds, returning data that is not
/// part of the disk at all.
#[async_test]
async fn read_past_end_does_not_return_footer() {
    let disk = Disk::new(
        BlobDisk::new_fixed_vhd1(FileBlob::new(fixed_vhd1_file()).unwrap())
            .await
            .unwrap(),
    )
    .unwrap();
    let mem = GuestMemory::allocate(SECTOR_SIZE);
    let r = disk
        .read_vectored(
            &OwnedRequestBuffers::linear(0, SECTOR_SIZE, true).buffer(&mem),
            disk.sector_count(),
        )
        .await;

    let mut buf = [0; SECTOR_SIZE];
    mem.read_at(0, &mut buf).unwrap();
    assert_ne!(&buf[..8], b"conectix", "read returned the VHD footer");
    assert!(matches!(r, Err(DiskError::IllegalBlock)), "{r:?}");
}

/// `BlobDisk` cannot tell a short read from a successful one by itself, so it
/// relies on the `Blob` contract requiring `UnexpectedEof` rather than partial
/// success. A misbehaving server must therefore surface as an error and never
/// as a read that quietly returns less, or the wrong, data.
///
/// These are responses a well-behaved server would not produce, which is why
/// they are worth testing: they are the cases the contract exists for.
#[async_test]
async fn misbehaving_server_never_reports_success() {
    let content = vec![0xab; DISK_SIZE as usize];
    for behavior in [
        Behavior::ShortBody,
        Behavior::IgnoreRange,
        Behavior::RangeNotSatisfiable,
        Behavior::CloseMidBody,
    ] {
        let server = TestHttpServer::new(content.clone(), behavior);
        let disk = Disk::new(BlobDisk::new(HttpBlob::new(&server.url()).await.unwrap())).unwrap();
        let mem = GuestMemory::allocate(SECTOR_SIZE);
        let r = disk
            .read_vectored(
                &OwnedRequestBuffers::linear(0, SECTOR_SIZE, true).buffer(&mem),
                0,
            )
            .await;

        match behavior {
            // The `Blob` documentation is specific about this one: a buffer
            // that is not completely filled must be reported as
            // `UnexpectedEof`, not as any kind of partial success. Assert the
            // documented error rather than merely that something went wrong,
            // since a short read succeeding is the failure mode that matters.
            Behavior::ShortBody => match r {
                Err(DiskError::Io(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {}
                r => panic!("{behavior:?}: expected UnexpectedEof, got {r:?}"),
            },
            _ => assert!(r.is_err(), "{behavior:?}: reported success"),
        }
    }
}
