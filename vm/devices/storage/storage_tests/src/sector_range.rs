// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Reusable sector-range conformance tests for [`Disk`] implementations.
//!
//! Storage frontends hand disk backends sector numbers that originate with the
//! guest, so every backend must cope with arbitrary [`u64`] sector values
//! without panicking, wrapping, or silently reading the wrong data. The suites
//! here exercise the awkward `(sector, length)` pairs that are easy to get
//! wrong, so that each backend does not have to reinvent the same table of
//! cases.
//!
//! There are two suites, matching the two distinct guarantees:
//!
//! * [`test_disk_representability`] covers requests whose end byte offset is
//!   not representable as a file offset. These are rejected by [`Disk`] itself,
//!   so every disk must pass this suite regardless of its backend.
//! * [`test_disk_sector_range`] covers requests that are representable but fall
//!   outside the disk. Only the backend can enforce this, since the sector
//!   count of some backends changes at runtime.
//!
//! Note that these tests are only meaningful with overflow checks enabled,
//! which is the default for the `dev` profile used by `cargo test` and
//! `cargo nextest`.

use disk_backend::Disk;
use disk_backend::DiskError;
use disk_backend::UnmapBehavior;
use disk_layered::LayerIo;
use disk_layered::WriteNoOverwrite;
use guestmem::GuestMemory;
use scsi_buffers::OwnedRequestBuffers;

/// The largest sector number that may appear as the *end* of a request, i.e.
/// the largest sector whose byte offset fits in an `i64`.
fn max_representable_sector(disk: &Disk) -> u64 {
    (i64::MAX as u64) >> disk.sector_shift()
}

#[track_caller]
fn expect_illegal_block(what: &str, r: Result<(), DiskError>) {
    match r {
        Err(DiskError::IllegalBlock) => {}
        Err(err) => panic!("{what}: expected IllegalBlock, got {err:?}"),
        Ok(()) => panic!("{what}: expected IllegalBlock, but the request succeeded"),
    }
}

/// Runs `case` against read, write, and unmap, asserting that each is rejected
/// with [`DiskError::IllegalBlock`].
///
/// Write is skipped for read-only disks. Unmap is *not* skipped for disks that
/// ignore unmap: a disk that does no work still has to reject a request that
/// names sectors it does not have, or the guest gets a success status for an
/// operation on sectors past the end of the disk.
async fn expect_rejected(disk: &Disk, mem: &GuestMemory, what: &str, sector: u64, count: u64) {
    let len = count as usize * disk.sector_size() as usize;

    expect_illegal_block(
        &format!("read: {what}"),
        disk.read_vectored(
            &OwnedRequestBuffers::linear(0, len, true).buffer(mem),
            sector,
        )
        .await,
    );

    if !disk.is_read_only() {
        expect_illegal_block(
            &format!("write: {what}"),
            disk.write_vectored(
                &OwnedRequestBuffers::linear(0, len, false).buffer(mem),
                sector,
                false,
            )
            .await,
        );
    }

    expect_illegal_block(
        &format!("unmap: {what}"),
        disk.unmap(sector, count, false).await,
    );
}

/// Asserts that `disk` rejects requests whose end byte offset is not
/// representable, without panicking or wrapping.
///
/// This is the guarantee that [`Disk`] itself provides, so every disk must pass
/// this suite. It deliberately says nothing about how big the disk is, so it
/// cannot be satisfied — or defeated — by the backend's own range checks.
pub async fn test_disk_representability(disk: &Disk) {
    let mem = GuestMemory::allocate(2 * disk.sector_size() as usize);
    let max = max_representable_sector(disk);

    // The sector number itself overflows when the length is added.
    expect_rejected(disk, &mem, "sector u64::MAX", u64::MAX, 1).await;
    expect_rejected(
        disk,
        &mem,
        "sector u64::MAX - 1, 2 sectors",
        u64::MAX - 1,
        2,
    )
    .await;

    // The sector number is fine, but the end byte offset is not representable.
    expect_rejected(disk, &mem, "one sector past the i64::MAX byte", max, 1).await;
    expect_rejected(disk, &mem, "straddling the i64::MAX byte", max - 1, 2).await;

    // A sector number that wraps to zero when shifted into a byte offset. This
    // is the case that silently returns the wrong data if a backend range check
    // is written in byte units without checking for lost bits.
    let wrapping_sector = 1 << (64 - disk.sector_shift());
    expect_rejected(
        disk,
        &mem,
        "sector that wraps when shifted",
        wrapping_sector,
        1,
    )
    .await;
}

/// Asserts that `disk` rejects representable requests that fall outside the
/// disk, and accepts the ones that fall inside it.
///
/// Only the backend can enforce this, since for several backends the sector
/// count can change at runtime, so a check made by a caller beforehand is not
/// authoritative.
pub async fn test_disk_sector_range(disk: &Disk) {
    let sector_size = disk.sector_size() as usize;
    let mem = GuestMemory::allocate(2 * sector_size);
    let sector_count = disk.sector_count();
    assert!(
        sector_count > 1,
        "test requires a disk of at least 2 sectors"
    );

    expect_rejected(disk, &mem, "first sector past the end", sector_count, 1).await;
    expect_rejected(disk, &mem, "straddling the end", sector_count - 1, 2).await;
    expect_rejected(
        disk,
        &mem,
        "largest representable sector",
        max_representable_sector(disk) - 1,
        1,
    )
    .await;

    // Sanity check that the boundary cases above are not passing because the
    // disk rejects everything.
    disk.read_vectored(
        &OwnedRequestBuffers::linear(0, sector_size, true).buffer(&mem),
        sector_count - 1,
    )
    .await
    .expect("read of the last sector should succeed");

    // A zero-length request at a valid sector is not out of range.
    disk.read_vectored(&OwnedRequestBuffers::linear(0, 0, true).buffer(&mem), 0)
        .await
        .expect("zero-length read should succeed");
}

/// Runs every sector-range conformance suite against `disk`.
pub async fn test_disk_sector_range_conformance(disk: &Disk) {
    test_disk_representability(disk).await;
    test_disk_sector_range(disk).await;
}

/// Asserts that a [`LayerIo`] implementation rejects requests that fall outside
/// the layer.
///
/// [`LayeredDisk`](disk_layered::LayeredDisk) does not reach every layer entry
/// point, so testing a layer only through a [`Disk`] leaves gaps:
/// `write_no_overwrite` is only reached by a read cache, and `unmap`'s
/// `next_is_zero` argument depends on what is in the layer below.
///
/// This deliberately stays within the representability guarantee documented on
/// [`LayerIo`]. A layer is entitled to assume the end byte offset of a request
/// is at most [`i64::MAX`], because [`Disk`] rejects anything larger and
/// `LayeredDisk` never increases a request's sector number. Calling a layer
/// directly with, say, [`u64::MAX`] would be breaking that precondition rather
/// than testing anything.
pub async fn test_layer_sector_range(layer: &impl LayerIo) {
    let sector_size = layer.sector_size() as usize;
    let mem = GuestMemory::allocate(2 * sector_size);
    let sector_count = layer.sector_count();
    assert!(
        sector_count > 1,
        "test requires a layer of at least 2 sectors"
    );
    let max_sector = (i64::MAX as u64) >> layer.sector_size().trailing_zeros();

    for (what, sector, count) in [
        ("first sector past the end", sector_count, 1u64),
        ("straddling the end", sector_count - 1, 2),
        ("largest representable sector", max_sector - 1, 1),
    ] {
        let len = count as usize * sector_size;

        expect_illegal_block(
            &format!("write: {what}"),
            layer
                .write(
                    &OwnedRequestBuffers::linear(0, len, false).buffer(&mem),
                    sector,
                    false,
                )
                .await,
        );

        if let Some(writer) = layer.write_no_overwrite() {
            expect_illegal_block(
                &format!("write_no_overwrite: {what}"),
                writer
                    .write_no_overwrite(
                        &OwnedRequestBuffers::linear(0, len, false).buffer(&mem),
                        sector,
                    )
                    .await,
            );
        }

        // A layer reporting `Ignored` makes the whole disk report `Ignored`, and
        // `LayeredDisk::unmap` then returns without calling any layer, so there
        // is no reachable path to test.
        if layer.unmap_behavior() != UnmapBehavior::Ignored {
            // `next_is_zero` selects how the layer represents the unmapped
            // range, so both values need checking: a range check guarding only
            // one of the two paths would otherwise look correct.
            for next_is_zero in [false, true] {
                expect_illegal_block(
                    &format!("unmap (next_is_zero={next_is_zero}): {what}"),
                    layer.unmap(sector, count, false, next_is_zero).await,
                );
            }
        }
    }

    // Sanity check that the cases above are not passing because the layer
    // rejects everything.
    layer
        .write(
            &OwnedRequestBuffers::linear(0, sector_size, false).buffer(&mem),
            sector_count - 1,
            false,
        )
        .await
        .expect("write to the last sector should succeed");
}
