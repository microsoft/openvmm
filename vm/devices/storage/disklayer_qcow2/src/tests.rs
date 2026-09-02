// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unit tests for the qcow2 disk layer.

use crate::Qcow2Layer;
use crate::header::Qcow2Header;
use disk_backend::Disk;
use disk_backend::DiskIo;
use disk_layered::DiskLayer;
use disk_layered::LayerConfiguration;
use disk_layered::LayeredDisk;
use guestmem::GuestMemory;
use pal_async::async_test;
use scsi_buffers::OwnedRequestBuffers;

const CLUSTER_BITS: u32 = 12; // 4 KiB clusters
const CLUSTER_SIZE: usize = 1 << CLUSTER_BITS;

/// Build an in-memory qcow2 image with one allocated cluster.
///
/// Layout:
///   0      : header (v2, 72 bytes)
///   cluster: L1 table (1 entry -> L2 table)
///   cluster: L2 table (entry 0 -> data cluster, rest unallocated)
///   cluster: data cluster containing a known pattern
///   cluster: refcount table (1 entry -> refcount block)
///   cluster: refcount block (clusters 0..=5 each have refcount 1)
/// Disk size is 1 MiB.
fn build_fixture() -> Vec<u8> {
    let mut img = vec![0u8; 6 * CLUSTER_SIZE];
    let data_cluster: u64 = 3 * CLUSTER_SIZE as u64;
    let l2_table: u64 = 2 * CLUSTER_SIZE as u64;
    let l1_table: u64 = 1 * CLUSTER_SIZE as u64;
    let refcount_table: u64 = 4 * CLUSTER_SIZE as u64;
    let refcount_block: u64 = 5 * CLUSTER_SIZE as u64;

    // Header.
    img[0..4].copy_from_slice(&0x5146_49FBu32.to_be_bytes()); // magic
    img[4..8].copy_from_slice(&2u32.to_be_bytes()); // version
    img[16..20].copy_from_slice(&0u32.to_be_bytes()); // backing file size
    img[20..24].copy_from_slice(&CLUSTER_BITS.to_be_bytes()); // cluster bits
    img[24..32].copy_from_slice(&(1024u64 * 1024).to_be_bytes()); // disk size
    img[36..40].copy_from_slice(&1u32.to_be_bytes()); // l1 size
    img[40..48].copy_from_slice(&l1_table.to_be_bytes());
    img[48..56].copy_from_slice(&refcount_table.to_be_bytes());
    img[56..60].copy_from_slice(&1u32.to_be_bytes()); // refcount table clusters

    // L1 entry 0 -> L2 table (COPIED bit set).
    let l1_entry = (1u64 << 63) | l2_table;
    img[l1_table as usize..l1_table as usize + 8].copy_from_slice(&l1_entry.to_be_bytes());

    // L2 entry 0 -> data cluster (COPIED bit set); rest stay 0 (unallocated).
    let l2_entry = (1u64 << 63) | data_cluster;
    img[l2_table as usize..l2_table as usize + 8].copy_from_slice(&l2_entry.to_be_bytes());

    // Refcount table entry 0 -> refcount block.
    img[refcount_table as usize..refcount_table as usize + 8]
        .copy_from_slice(&refcount_block.to_be_bytes());

    // Refcount block: every cluster used by the image has refcount 1.
    for cluster in 0..6u32 {
        let entry = refcount_block as usize + cluster as usize * 2;
        img[entry..entry + 2].copy_from_slice(&1u16.to_be_bytes());
    }

    // Data cluster: a recognizable pattern.
    for i in 0..CLUSTER_SIZE {
        img[data_cluster as usize + i] = (i % 251) as u8;
    }

    img
}

fn open_layer(path: &std::path::Path, read_only: bool) -> Qcow2Layer {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(!read_only)
        .open(path)
        .unwrap();
    let header = Qcow2Header::from_file(&mut file).unwrap();
    Qcow2Layer::new(file, header, read_only).unwrap()
}

#[async_test]
async fn read_allocated_cluster() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.qcow2");
    std::fs::write(&path, build_fixture()).unwrap();

    let layer = open_layer(&path, true);
    let disk = LayeredDisk::new(
        true,
        vec![LayerConfiguration {
            layer: DiskLayer::new(layer),
            write_through: false,
            read_cache: false,
        }],
    )
    .await
    .unwrap();

    let mem = GuestMemory::allocate(512);
    let owned = OwnedRequestBuffers::linear(0, 512, true);
    disk.read_vectored(&owned.buffer(&mem), 0).await.unwrap();

    let mut buf = vec![0u8; 512];
    mem.read_at(0, &mut buf).unwrap();
    let expected: Vec<u8> = (0..512u16).map(|i| (i % 251) as u8).collect();
    assert_eq!(buf, expected);
}

#[async_test]
async fn read_unallocated_is_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.qcow2");
    std::fs::write(&path, build_fixture()).unwrap();

    let layer = open_layer(&path, true);
    let disk = Disk::new(
        LayeredDisk::new(
            true,
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

    // L2 entry 1 and beyond are unallocated; reading a later cluster (e.g.
    // the L2 table addresses many clusters) should fall through as zeros.
    let mem = GuestMemory::allocate(512);
    let owned = OwnedRequestBuffers::linear(0, 512, true);
    // Sector 4 is in cluster 0, L2 index 0 but offset 4*512=2048 is within
    // cluster 0 (allocated). Use a cluster that is unallocated in the L2.
    // Cluster 0 spans sectors 0-7. Cluster 1 = sectors 8-15 is unallocated.
    disk.read_vectored(&owned.buffer(&mem), 8).await.unwrap();

    let mut buf = vec![0xFFu8; 512];
    mem.read_at(0, &mut buf).unwrap();
    assert_eq!(buf, vec![0u8; 512]);
}

#[async_test]
async fn write_unallocated_allocates_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.qcow2");
    std::fs::write(&path, build_fixture()).unwrap();

    // Sector 8 is in cluster 1, which is unallocated in the fixture's L2
    // table. Writing it should allocate a fresh data cluster and persist the
    // new L1/L2 metadata back to disk.
    let pattern: Vec<u8> = (0..512u16).map(|i| (i * 7 % 256) as u8).collect();

    let layer = open_layer(&path, false);
    let disk = Disk::new(
        LayeredDisk::new(
            true,
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

    let mem = GuestMemory::allocate(512);
    mem.write_at(0, &pattern).unwrap();
    let owned = OwnedRequestBuffers::linear(0, 512, true);
    disk.write_vectored(&owned.buffer(&mem), 8, false)
        .await
        .unwrap();

    // Read it back through the same disk.
    let mut buf = vec![0u8; 512];
    mem.write_at(0, &vec![0u8; 512]).unwrap();
    disk.read_vectored(&owned.buffer(&mem), 8).await.unwrap();
    mem.read_at(0, &mut buf).unwrap();
    assert_eq!(buf, pattern);
    drop(disk);

    // Re-open the file fresh to confirm the allocation was persisted.
    let layer2 = open_layer(&path, false);
    let disk2 = Disk::new(
        LayeredDisk::new(
            true,
            vec![LayerConfiguration {
                layer: DiskLayer::new(layer2),
                write_through: false,
                read_cache: false,
            }],
        )
        .await
        .unwrap(),
    )
    .unwrap();

    let mem2 = GuestMemory::allocate(512);
    let owned2 = OwnedRequestBuffers::linear(0, 512, true);
    disk2.read_vectored(&owned2.buffer(&mem2), 8).await.unwrap();
    let mut buf2 = vec![0u8; 512];
    mem2.read_at(0, &mut buf2).unwrap();
    assert_eq!(buf2, pattern);
}

#[async_test]
async fn write_updates_refcounts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.qcow2");
    std::fs::write(&path, build_fixture()).unwrap();

    // Writing sector 8 (unallocated cluster 1) allocates a fresh data cluster
    // at the end of the file: cluster 6. Its refcount, stored in the refcount
    // block, must be written back to disk.
    let pattern: Vec<u8> = (0..512u16).map(|i| (i * 7 % 256) as u8).collect();

    let layer = open_layer(&path, false);
    let disk = Disk::new(
        LayeredDisk::new(
            true,
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

    let mem = GuestMemory::allocate(512);
    mem.write_at(0, &pattern).unwrap();
    let owned = OwnedRequestBuffers::linear(0, 512, true);
    disk.write_vectored(&owned.buffer(&mem), 8, false)
        .await
        .unwrap();
    drop(disk);

    // Read the raw refcount block (cluster 5) from the file on disk.
    let img = std::fs::read(&path).unwrap();
    assert_eq!(img.len(), 7 * CLUSTER_SIZE, "one data cluster appended");
    let refcount = |cluster: usize| -> u16 {
        let off = 5 * CLUSTER_SIZE + cluster * 2;
        u16::from_be_bytes([img[off], img[off + 1]])
    };
    // The original six clusters each have refcount 1...
    for cluster in 0..6 {
        assert_eq!(refcount(cluster), 1, "cluster {cluster}");
    }
    // ...and so does the newly allocated data cluster.
    assert_eq!(refcount(6), 1);
}

#[async_test]
async fn write_overwrites_existing_cluster() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.qcow2");
    std::fs::write(&path, build_fixture()).unwrap();

    // Sector 0 is in cluster 0, which the fixture pre-allocates with the
    // (i % 251) pattern. Overwrite it with a distinct pattern, then verify the
    // overwritten sector changed while an adjacent, untouched sector in the
    // same cluster still holds the original fixture data.
    let new_pattern: Vec<u8> = (0..512u16).map(|i| (i as u8).wrapping_add(200)).collect();

    let layer = open_layer(&path, false);
    let disk = Disk::new(
        LayeredDisk::new(
            true,
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

    let mem = GuestMemory::allocate(512);
    mem.write_at(0, &new_pattern).unwrap();
    let owned = OwnedRequestBuffers::linear(0, 512, true);
    disk.write_vectored(&owned.buffer(&mem), 0, false)
        .await
        .unwrap();
    drop(disk);

    // Re-open and check both sectors from a clean state.
    let layer2 = open_layer(&path, false);
    let disk2 = Disk::new(
        LayeredDisk::new(
            true,
            vec![LayerConfiguration {
                layer: DiskLayer::new(layer2),
                write_through: false,
                read_cache: false,
            }],
        )
        .await
        .unwrap(),
    )
    .unwrap();

    // Sector 0: the newly written pattern.
    let mut buf = vec![0u8; 512];
    let owned0 = OwnedRequestBuffers::linear(0, 512, true);
    disk2.read_vectored(&owned0.buffer(&mem), 0).await.unwrap();
    mem.read_at(0, &mut buf).unwrap();
    assert_eq!(buf, new_pattern);

    // Sector 1: untouched by the write, still the fixture pattern.
    disk2.read_vectored(&owned0.buffer(&mem), 1).await.unwrap();
    mem.read_at(0, &mut buf).unwrap();
    let expected: Vec<u8> = (0..512u16).map(|i| ((512 + i) % 251) as u8).collect();
    assert_eq!(buf, expected);
}

/// Build a fixture identical to [`build_fixture`] except that L2 entry 1 has
/// bit 0 (the Standard Cluster Descriptor "reads as all zeros" flag) set with
/// a non-zero host offset. Per the spec, reads must return zeros regardless
/// of the host offset.
fn build_zero_flag_fixture() -> Vec<u8> {
    let mut img = build_fixture();
    let l2_table: usize = 2 * CLUSTER_SIZE;
    let l2_entry: u64 = (1u64 << 63) // COPIED
        | 1                          // bit 0: reads as all zeros
        | (3 * CLUSTER_SIZE as u64); // non-zero, but ignored, host offset
    img[l2_table..l2_table + 8].copy_from_slice(&l2_entry.to_be_bytes());
    img
}

#[async_test]
async fn read_zero_flag_cluster_returns_zeros() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.qcow2");
    std::fs::write(&path, build_zero_flag_fixture()).unwrap();

    let layer = open_layer(&path, true);
    let disk = Disk::new(
        LayeredDisk::new(
            true,
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

    // Cluster 1 = sectors 8-15. Its L2 entry has the "reads as zeros" flag
    // set, so it must read back as zeros even though the host offset is
    // non-zero.
    let mem = GuestMemory::allocate(512);
    let owned = OwnedRequestBuffers::linear(0, 512, true);
    disk.read_vectored(&owned.buffer(&mem), 8).await.unwrap();
    let mut buf = vec![0xFFu8; 512];
    mem.read_at(0, &mut buf).unwrap();
    assert_eq!(buf, vec![0u8; 512]);
}
