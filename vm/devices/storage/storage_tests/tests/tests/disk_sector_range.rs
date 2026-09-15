// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Sector-range validation tests that span more than one crate.
//!
//! Each backend runs the shared conformance suite from its own crate; see
//! `storage_tests::sector_range`. The cases here are the ones that cannot,
//! because constructing the disk needs crates the backend does not itself
//! depend on, or because the backend is one the suite depends on.

use disk_backend::Disk;
use disk_layered::DiskLayer;
use disk_layered::LayerConfiguration;
use disk_layered::LayeredDisk;
use disklayer_ram::RamDiskLayer;
#[cfg(any(windows, target_os = "linux"))]
use pal_async::DefaultDriver;
use pal_async::async_test;
use storage_tests::sector_range::test_disk_sector_range_conformance;
use test_with_tracing::test;

/// Only the NVMe test needs this, and it is not built everywhere.
#[cfg(any(windows, target_os = "linux"))]
const SECTOR_SIZE: u64 = 512;
const DISK_SIZE: u64 = 1024 * 1024;

fn ram_layer_config(read_cache: bool) -> LayerConfiguration {
    LayerConfiguration {
        layer: DiskLayer::new(RamDiskLayer::new(DISK_SIZE).unwrap()),
        write_through: false,
        read_cache,
    }
}

/// `disk_layered` cannot run the suite from its own crate: `storage_tests`
/// depends on it, so dev-depending back on `storage_tests` would put a second
/// copy of `disk_layered` in the graph and its `LayerIo` impls would not match
/// the instance under test.
async fn layered_disk(read_cache: bool) -> Disk {
    Disk::new(
        LayeredDisk::new(
            false,
            vec![ram_layer_config(read_cache), ram_layer_config(false)],
        )
        .await
        .unwrap(),
    )
    .unwrap()
}

/// Two layers, so the paths that consult the layer below are exercised.
#[async_test]
async fn layered_multi_sector_range_conformance() {
    test_disk_sector_range_conformance(&layered_disk(false).await).await;
}

/// A read cache is the only configuration that reaches
/// `LayerIo::write_no_overwrite`.
#[async_test]
async fn layered_read_cache_sector_range_conformance() {
    test_disk_sector_range_conformance(&layered_disk(true).await).await;
}

/// `disk_nvme` needs an emulated controller and PCI device to construct, which
/// live in crates it does not depend on, so this composition only exists here.
///
/// It is also the case where the range check is legitimately delegated to the
/// device that owns the storage, so it checks that the controller's
/// `LBA_OUT_OF_RANGE` survives the trip back as `DiskError::IllegalBlock`.
#[cfg(any(windows, target_os = "linux"))]
#[async_test]
async fn nvme_sector_range_conformance(driver: DefaultDriver) {
    let mut nvme = crate::emulated_nvme::EmulatedNvme::new(
        driver,
        SECTOR_SIZE as u32,
        DISK_SIZE / SECTOR_SIZE,
        false,
        "storage_tests_sector_range_nvme",
    )
    .await;
    let disk = nvme.disk().await;
    test_disk_sector_range_conformance(&disk).await;
}
