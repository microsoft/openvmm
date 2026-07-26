// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Sector-range validation tests for disk backends.
//!
//! Storage frontends pass guest-supplied sector numbers through to disk
//! backends, so every backend must reject out-of-range and non-representable
//! requests rather than panicking or silently operating on the wrong data.
//!
//! The file is in three parts: fixtures constructing each disk under test, a
//! `conformance` module whose entries are the authoritative list of which
//! backends run the shared suite, and targeted tests for individual defects.
//!
//! These tests are only meaningful with overflow checks enabled, which is the
//! default for the `dev` profile used by `cargo test` and `cargo nextest`.

use disk_backend::Disk;
use disk_backend::DiskError;
use disk_blob::BlobDisk;
use disk_blob::blob::file::FileBlob;
use disk_crypt::CryptDisk;
use disk_delay::DelayDisk;
use disk_file::FileDisk;
use disk_layered::DiskLayer;
use disk_layered::LayerConfiguration;
use disk_layered::LayeredDisk;
use disk_prwrap::DiskWithReservations;
use disk_striped::StripedDisk;
use disk_vhd1::Vhd1Disk;
use disklayer_ram::RamDiskLayer;
use disklayer_sqlite::FormatParams;
use disklayer_sqlite::SqliteDiskLayer;
use guestmem::GuestMemory;
use pal_async::DefaultDriver;
use pal_async::async_test;
use scsi_buffers::OwnedRequestBuffers;
use std::time::Duration;
use storage_tests::sector_range::test_disk_representability;
use storage_tests::sector_range::test_disk_sector_range_conformance;
use test_with_tracing::test;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;

const SECTOR_SIZE: u64 = 512;
const DISK_SIZE: u64 = 1024 * 1024;

// --------------------------------------------------------------------------
// Fixtures
// --------------------------------------------------------------------------

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

/// A file containing a fixed VHD1 image of `DISK_SIZE` bytes, i.e. `DISK_SIZE`
/// bytes of data followed by a 512-byte footer.
fn fixed_vhd1_file() -> std::fs::File {
    let file = tempfile::tempfile().unwrap();
    file.set_len(DISK_SIZE).unwrap();
    Vhd1Disk::make_fixed(&file).unwrap();
    file
}

fn vhd1_disk() -> Disk {
    Disk::new(Vhd1Disk::open_fixed(fixed_vhd1_file(), false).unwrap()).unwrap()
}

fn crypt_disk() -> Disk {
    // XTS requires the two halves of the key to differ; a uniform key is
    // rejected by the crypto backend at cipher init, which would make every
    // write fail for a reason that has nothing to do with the sector range.
    let mut key = [0; 64];
    key[..32].fill(0xab);
    key[32..].fill(0xcd);
    Disk::new(CryptDisk::new(disk_crypt_resources::Cipher::XtsAes256, &key, ram_disk()).unwrap())
        .unwrap()
}

fn prwrap_disk() -> Disk {
    Disk::new(DiskWithReservations::new(ram_disk())).unwrap()
}

fn delay_disk(driver: DefaultDriver) -> Disk {
    let source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    // A zero delay keeps the test fast. The delay is never updated, so the
    // updater can be dropped immediately; the cell retains its initial value.
    let cell = mesh::CellUpdater::new(Duration::ZERO).cell();
    Disk::new(DelayDisk::new(cell, ram_disk(), &source)).unwrap()
}

/// A blob whose length is exactly the disk size, so that the backing object's
/// bounds coincide with the disk's.
fn blob_raw_disk() -> Disk {
    let file = tempfile::tempfile().unwrap();
    file.set_len(DISK_SIZE).unwrap();
    Disk::new(BlobDisk::new(FileBlob::new(file).unwrap())).unwrap()
}

/// A blob that is strictly larger than the disk it presents, because of the
/// trailing VHD footer.
async fn blob_vhd1_disk() -> Disk {
    Disk::new(
        BlobDisk::new_fixed_vhd1(FileBlob::new(fixed_vhd1_file()).unwrap())
            .await
            .unwrap(),
    )
    .unwrap()
}

/// A SQLite-backed layer in a temporary directory, which is returned alongside
/// the disk so that it outlives it.
async fn sqlite_disk() -> (Disk, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let layer = SqliteDiskLayer::new(
        &dir.path().join("test.dbhd"),
        false,
        Some(FormatParams {
            logically_read_only: false,
            len: DISK_SIZE,
            sector_size: SECTOR_SIZE as u32,
        }),
    )
    .unwrap();
    let disk = Disk::new(
        LayeredDisk::new(
            false,
            vec![LayerConfiguration {
                layer: DiskLayer::new(layer),
                write_through: false,
                read_cache: false,
            }],
        )
        .await
        .unwrap(),
    )
    .unwrap();
    (disk, dir)
}

fn ram_layer_config(write_through: bool, read_cache: bool) -> LayerConfiguration {
    LayerConfiguration {
        layer: DiskLayer::new(RamDiskLayer::new(DISK_SIZE).unwrap()),
        write_through,
        read_cache,
    }
}

/// A namespace on an emulated NVMe controller. The controller is returned
/// alongside the disk because the driver behind the namespace must outlive it.
#[cfg(any(windows, target_os = "linux"))]
async fn nvme_disk(driver: DefaultDriver) -> (Disk, crate::common::EmulatedNvme) {
    let mut nvme = crate::common::EmulatedNvme::new(
        driver,
        SECTOR_SIZE as u32,
        DISK_SIZE / SECTOR_SIZE,
        false,
        "disk_sector_range_nvme",
    )
    .await;
    let disk = nvme.disk().await;
    (disk, nvme)
}

/// Two RAM layers, so that the code paths that consult the layer below a given
/// layer are exercised.
async fn layered_multi_disk() -> Disk {
    Disk::new(
        LayeredDisk::new(
            false,
            vec![
                ram_layer_config(false, false),
                ram_layer_config(false, false),
            ],
        )
        .await
        .unwrap(),
    )
    .unwrap()
}

/// A read-cache configuration, which is the only way to reach
/// `LayerIo::write_no_overwrite`.
async fn layered_read_cache_disk() -> Disk {
    Disk::new(
        LayeredDisk::new(
            false,
            vec![
                ram_layer_config(false, true),
                ram_layer_config(false, false),
            ],
        )
        .await
        .unwrap(),
    )
    .unwrap()
}

// --------------------------------------------------------------------------
// Conformance coverage
// --------------------------------------------------------------------------

/// A disk under test, together with anything that must outlive it.
struct Fixture {
    disk: Disk,
    _keepalive: Option<Box<dyn Send>>,
}

impl From<Disk> for Fixture {
    fn from(disk: Disk) -> Self {
        Self {
            disk,
            _keepalive: None,
        }
    }
}

/// For disks that depend on something outliving them — a temporary directory,
/// or the NVMe driver behind a namespace.
impl<T: Send + 'static> From<(Disk, T)> for Fixture {
    fn from((disk, keepalive): (Disk, T)) -> Self {
        Self {
            disk,
            _keepalive: Some(Box::new(keepalive)),
        }
    }
}

/// Declares one conformance test per entry.
///
/// Each entry is a test name and an expression producing the disk to test. The
/// expression may `.await`, and may use the driver named by the leading
/// `|driver|` binding. It evaluates to either a [`Disk`], or a `(Disk, T)` pair
/// when something must be kept alive for the duration of the test.
macro_rules! conformance_tests {
    (|$driver:ident| $(
        $(#[$meta:meta])*
        $name:ident => $disk:expr;
    )*) => {
        $(
            $(#[$meta])*
            #[async_test]
            async fn $name($driver: DefaultDriver) {
                let _ = &$driver;
                let fixture: Fixture = ($disk).into();
                test_disk_sector_range_conformance(&fixture.disk).await;
            }
        )*
    };
}

/// The set of backends covered by the shared sector-range conformance suite.
///
/// This list is the coverage statement for this file — a backend absent from it
/// is not tested.
mod conformance {
    use super::*;
    use test_with_tracing::test;

    conformance_tests! {
        |driver|
        /// Host file.
        file => file_disk();
        /// Fixed VHD1 in a host file.
        vhd1 => vhd1_disk();
        /// Read-only blob whose length is exactly the disk size.
        blob_raw => blob_raw_disk();
        /// Read-only blob strictly larger than the disk it presents.
        blob_fixed_vhd1 => blob_vhd1_disk().await;
        /// A single RAM layer.
        ram => ram_disk();
        /// A single SQLite layer.
        sqlite => sqlite_disk().await;
        /// Two RAM layers, exercising the paths that consult the layer below.
        layered_multi => layered_multi_disk().await;
        /// Two RAM layers with the upper one a read cache, which is the only
        /// way to reach `LayerIo::write_no_overwrite`.
        layered_read_cache => layered_read_cache_disk().await;
        /// Striped across two RAM disks.
        striped => striped_disk();
        /// A namespace on an emulated NVMe controller. This is the case where
        /// the range check is legitimately delegated to the device that owns
        /// the storage, so it also checks that the controller's
        /// `LBA_OUT_OF_RANGE` survives the trip back as `IllegalBlock`.
        #[cfg(any(windows, target_os = "linux"))]
        nvme => nvme_disk(driver).await;
        /// Encryption wrapper over a RAM disk.
        crypt => crypt_disk();
        /// Persistent-reservation wrapper over a RAM disk.
        prwrap => prwrap_disk();
        /// Latency-injection wrapper over a RAM disk.
        delay => delay_disk(driver);
    }
}

// --------------------------------------------------------------------------
// Targeted tests for individual defects
// --------------------------------------------------------------------------

/// A blob larger than the disk it presents is the case where delegating the
/// range check to the backing object is not sufficient: a read one sector past
/// the end lands in the VHD footer and succeeds, returning data that is not
/// part of the disk at all.
#[async_test]
async fn blob_vhd1_read_past_end_does_not_return_footer() {
    let disk = blob_vhd1_disk().await;
    let mem = GuestMemory::allocate(SECTOR_SIZE as usize);
    let r = disk
        .read_vectored(
            &OwnedRequestBuffers::linear(0, SECTOR_SIZE as usize, true).buffer(&mem),
            disk.sector_count(),
        )
        .await;

    let mut buf = [0; SECTOR_SIZE as usize];
    mem.read_at(0, &mut buf).unwrap();
    assert_ne!(&buf[..8], b"conectix", "read returned the VHD footer");
    assert!(matches!(r, Err(DiskError::IllegalBlock)), "{r:?}");
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
