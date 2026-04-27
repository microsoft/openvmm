// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::AsyncFileExt;
use crate::create::{self, CreateParams};
use crate::error::VhdxIoError;
use crate::error::VhdxIoErrorInner;
use crate::format;
use crate::format::BatEntryState;
use crate::format::MB1;
use crate::io::ReadRange;
use crate::io::WriteRange;
use crate::open::VhdxFile;
use crate::tests::support::InMemoryFile;
use crate::trim::{TrimMode, TrimRequest};
use pal_async::DefaultDriver;
use pal_async::async_test;

/// Helper to create a disk and write a full block, returning the VhdxFile.
async fn create_and_write_block(
    disk_size: u64,
    block_number: u32,
    driver: &DefaultDriver,
) -> VhdxFile<InMemoryFile> {
    let (file, _) = InMemoryFile::create_test_vhdx(disk_size).await;
    let vhdx = VhdxFile::open(file).writable(driver).await.unwrap();
    let block_offset = block_number as u64 * vhdx.block_size() as u64;
    let block_size = vhdx.block_size();

    // Write a full block of data.
    let mut ranges = Vec::new();
    let guard = vhdx
        .resolve_write(block_offset, block_size, &mut ranges)
        .await
        .unwrap();

    // Perform the writes (we don't actually need to write data for BAT testing).
    for range in &ranges {
        match range {
            WriteRange::Data {
                file_offset,
                length,
                ..
            } => {
                let buf = vec![0xAA; *length as usize];
                vhdx.file.write_at(*file_offset, &buf).await.unwrap();
            }
            WriteRange::Zero {
                file_offset,
                length,
            } => {
                let buf = vec![0u8; *length as usize];
                vhdx.file.write_at(*file_offset, &buf).await.unwrap();
            }
        }
    }
    guard.complete().await.unwrap();
    vhdx
}

/// Helper to verify a block's BAT state.
fn assert_block_state(vhdx: &VhdxFile<InMemoryFile>, block_number: u32, expected: BatEntryState) {
    let mapping = vhdx.bat.get_block_mapping(block_number);
    let actual = mapping.bat_state();
    assert_eq!(
        actual, expected,
        "block {block_number}: expected {expected:?}, got {actual:?}"
    );
}

/// Helper to check if a block has a non-zero file megabyte (soft anchor).
fn block_has_file_offset(vhdx: &VhdxFile<InMemoryFile>, block_number: u32) -> bool {
    vhdx.bat.get_block_mapping(block_number).file_megabyte() != 0
}

/// Helper to write data to ranges returned by resolve_write.
async fn write_ranges(vhdx: &VhdxFile<InMemoryFile>, ranges: &[WriteRange], pattern: u8) {
    for range in ranges {
        match range {
            WriteRange::Data {
                file_offset,
                length,
                ..
            } => {
                let buf = vec![pattern; *length as usize];
                vhdx.file.write_at(*file_offset, &buf).await.unwrap();
            }
            WriteRange::Zero {
                file_offset,
                length,
            } => {
                let buf = vec![0u8; *length as usize];
                vhdx.file.write_at(*file_offset, &buf).await.unwrap();
            }
        }
    }
}

// ---- Basic Trim Tests ----

#[async_test]
async fn trim_full_block_file_space(driver: DefaultDriver) {
    let vhdx = create_and_write_block(format::GB1, 0, &driver).await;
    assert_block_state(&vhdx, 0, BatEntryState::FullyPresent);
    assert!(block_has_file_offset(&vhdx, 0));

    vhdx.trim(TrimRequest::new(
        TrimMode::FileSpace,
        0,
        vhdx.block_size() as u64,
    ))
    .await
    .unwrap();

    assert_block_state(&vhdx, 0, BatEntryState::Unmapped);
    // Soft anchor preserved.
    assert!(block_has_file_offset(&vhdx, 0));
}

#[async_test]
async fn trim_full_block_free_space(driver: DefaultDriver) {
    let vhdx = create_and_write_block(format::GB1, 0, &driver).await;

    vhdx.trim(TrimRequest::new(
        TrimMode::FreeSpace,
        0,
        vhdx.block_size() as u64,
    ))
    .await
    .unwrap();

    assert_block_state(&vhdx, 0, BatEntryState::Undefined);
    // FreeSpace on FullyPresent clears file offset (releases space).
    assert!(!block_has_file_offset(&vhdx, 0));
}

#[async_test]
async fn trim_full_block_zero(driver: DefaultDriver) {
    let vhdx = create_and_write_block(format::GB1, 0, &driver).await;

    vhdx.trim(TrimRequest::new(
        TrimMode::Zero,
        0,
        vhdx.block_size() as u64,
    ))
    .await
    .unwrap();

    assert_block_state(&vhdx, 0, BatEntryState::Zero);
    assert!(!block_has_file_offset(&vhdx, 0));
}

#[async_test]
async fn trim_full_block_make_transparent(driver: DefaultDriver) {
    let vhdx = create_and_write_block(format::GB1, 0, &driver).await;

    vhdx.trim(TrimRequest::new(
        TrimMode::MakeTransparent,
        0,
        vhdx.block_size() as u64,
    ))
    .await
    .unwrap();

    assert_block_state(&vhdx, 0, BatEntryState::NotPresent);
    assert!(!block_has_file_offset(&vhdx, 0));
}

#[async_test]
async fn trim_remove_soft_anchors(driver: DefaultDriver) {
    let vhdx = create_and_write_block(format::GB1, 0, &driver).await;

    // First trim with FileSpace to create a soft anchor.
    vhdx.trim(TrimRequest::new(
        TrimMode::FileSpace,
        0,
        vhdx.block_size() as u64,
    ))
    .await
    .unwrap();
    assert_block_state(&vhdx, 0, BatEntryState::Unmapped);
    assert!(block_has_file_offset(&vhdx, 0));

    // Now remove the soft anchor.
    vhdx.trim(TrimRequest::new(
        TrimMode::RemoveSoftAnchors,
        0,
        vhdx.block_size() as u64,
    ))
    .await
    .unwrap();
    assert_block_state(&vhdx, 0, BatEntryState::Unmapped);
    assert!(!block_has_file_offset(&vhdx, 0));
}

#[async_test]
async fn trim_already_trimmed_idempotent(driver: DefaultDriver) {
    let vhdx = create_and_write_block(format::GB1, 0, &driver).await;

    vhdx.trim(TrimRequest::new(
        TrimMode::FileSpace,
        0,
        vhdx.block_size() as u64,
    ))
    .await
    .unwrap();
    assert_block_state(&vhdx, 0, BatEntryState::Unmapped);

    // Second trim with FileSpace → no-op.
    vhdx.trim(TrimRequest::new(
        TrimMode::FileSpace,
        0,
        vhdx.block_size() as u64,
    ))
    .await
    .unwrap();
    assert_block_state(&vhdx, 0, BatEntryState::Unmapped);
}

#[async_test]
async fn trim_undefined_block_file_space_noop(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    // Block 0 starts as NotPresent on a fresh non-differencing disk.
    assert_block_state(&vhdx, 0, BatEntryState::NotPresent);

    vhdx.trim(TrimRequest::new(
        TrimMode::FileSpace,
        0,
        vhdx.block_size() as u64,
    ))
    .await
    .unwrap();

    // FileSpace is a no-op for NotPresent → should still be NotPresent.
    assert_block_state(&vhdx, 0, BatEntryState::NotPresent);
}

#[async_test]
async fn trim_zero_block_noop(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    // First: write and trim to Zero to get a Zero block.
    let block_size = vhdx.block_size();
    let mut ranges = Vec::new();
    let guard = vhdx
        .resolve_write(0, block_size, &mut ranges)
        .await
        .unwrap();
    write_ranges(&vhdx, &ranges, 0).await;
    guard.complete().await.unwrap();

    vhdx.trim(TrimRequest::new(TrimMode::Zero, 0, block_size as u64))
        .await
        .unwrap();
    assert_block_state(&vhdx, 0, BatEntryState::Zero);

    // Second Zero trim → no-op.
    vhdx.trim(TrimRequest::new(TrimMode::Zero, 0, block_size as u64))
        .await
        .unwrap();
    assert_block_state(&vhdx, 0, BatEntryState::Zero);
}

// ---- Range Tests ----

#[async_test]
async fn trim_cross_block(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let bs = vhdx.block_size();

    // Write blocks 0, 1, 2.
    for block in 0..3u32 {
        let offset = block as u64 * bs as u64;
        let mut ranges = Vec::new();
        let guard = vhdx.resolve_write(offset, bs, &mut ranges).await.unwrap();
        write_ranges(&vhdx, &ranges, 0xBB).await;
        guard.complete().await.unwrap();
    }

    // Trim all 3 blocks at once.
    vhdx.trim(TrimRequest::new(TrimMode::FileSpace, 0, 3 * bs as u64))
        .await
        .unwrap();

    for block in 0..3u32 {
        assert_block_state(&vhdx, block, BatEntryState::Unmapped);
        assert!(block_has_file_offset(&vhdx, block));
    }
}

#[async_test]
async fn trim_partial_range_skips_edges(driver: DefaultDriver) {
    let file = InMemoryFile::new(0);
    let bs = MB1 as u32; // Use 1 MiB blocks for easier testing.
    let mut params = CreateParams {
        disk_size: 4 * MB1,
        block_size: bs,
        ..Default::default()
    };
    create::create(&file, &mut params).await.unwrap();
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    // Write blocks 0, 1, 2.
    for block in 0..3u32 {
        let offset = block as u64 * bs as u64;
        let mut ranges = Vec::new();
        let guard = vhdx.resolve_write(offset, bs, &mut ranges).await.unwrap();
        write_ranges(&vhdx, &ranges, 0xCC).await;
        guard.complete().await.unwrap();
    }

    // Trim from mid-block-0 through mid-block-2 → only block 1 is trimmed.
    let trim_offset = MB1 / 2; // mid-block-0
    let trim_length = 2 * MB1; // covers block 1 fully, partial block 0 and 2
    vhdx.trim(TrimRequest::new(
        TrimMode::FileSpace,
        trim_offset,
        trim_length,
    ))
    .await
    .unwrap();

    assert_block_state(&vhdx, 0, BatEntryState::FullyPresent); // partial → not trimmed
    assert_block_state(&vhdx, 1, BatEntryState::Unmapped); // fully covered → trimmed
    assert_block_state(&vhdx, 2, BatEntryState::FullyPresent); // partial → not trimmed
}

#[async_test]
async fn trim_entire_disk(driver: DefaultDriver) {
    let file = InMemoryFile::new(0);
    let bs = MB1 as u32;
    let mut params = CreateParams {
        disk_size: 4 * MB1,
        block_size: bs,
        ..Default::default()
    };
    create::create(&file, &mut params).await.unwrap();
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    // Write all 4 blocks.
    for block in 0..4u32 {
        let offset = block as u64 * bs as u64;
        let mut ranges = Vec::new();
        let guard = vhdx.resolve_write(offset, bs, &mut ranges).await.unwrap();
        write_ranges(&vhdx, &ranges, 0xDD).await;
        guard.complete().await.unwrap();
    }

    // Trim the entire disk.
    vhdx.trim(TrimRequest::new(TrimMode::FileSpace, 0, 4 * MB1))
        .await
        .unwrap();

    for block in 0..4u32 {
        assert_block_state(&vhdx, block, BatEntryState::Unmapped);
    }
}

#[async_test]
async fn trim_at_disk_end_rounds_up(driver: DefaultDriver) {
    // The disk size may not be an exact multiple of block size.
    // If trim range ends exactly at disk_size, we round up.
    let file = InMemoryFile::new(0);
    let bs = MB1 as u32;
    // 3.5 MiB disk with 1 MiB blocks → 4 blocks (last block is partial).
    let disk_size = 3 * MB1 + MB1 / 2;
    let mut params = CreateParams {
        disk_size,
        block_size: bs,
        ..Default::default()
    };
    create::create(&file, &mut params).await.unwrap();
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    // Write block 3 (the last, partial block).
    let block3_offset = 3 * MB1;
    // Write less than a full block (only the valid portion).
    let write_size = (MB1 / 2) as u32;
    let mut ranges = Vec::new();
    let guard = vhdx
        .resolve_write(block3_offset, write_size, &mut ranges)
        .await
        .unwrap();
    write_ranges(&vhdx, &ranges, 0xEE).await;
    guard.complete().await.unwrap();

    // Trim from block 3 to end of disk.
    vhdx.trim(TrimRequest::new(
        TrimMode::FileSpace,
        block3_offset,
        disk_size - block3_offset,
    ))
    .await
    .unwrap();

    // Block 3 should be trimmed (disk end rounding kicks in).
    assert_block_state(&vhdx, 3, BatEntryState::Unmapped);
}

// ---- Read-After-Trim Tests ----

#[async_test]
async fn read_after_trim_returns_zeros(driver: DefaultDriver) {
    let vhdx = create_and_write_block(format::GB1, 0, &driver).await;

    // Verify data is present.
    let mut ranges = Vec::new();
    let guard = vhdx.resolve_read(0, 4096, &mut ranges).await.unwrap();
    assert!(matches!(ranges[0], ReadRange::Data { .. }));
    drop(guard);

    // Trim.
    vhdx.trim(TrimRequest::new(
        TrimMode::FileSpace,
        0,
        vhdx.block_size() as u64,
    ))
    .await
    .unwrap();

    // Read after trim → zeros.
    let mut ranges = Vec::new();
    let _guard = vhdx.resolve_read(0, 4096, &mut ranges).await.unwrap();
    assert_eq!(ranges.len(), 1);
    assert!(
        matches!(ranges[0], ReadRange::Zero { .. }),
        "expected Zero range after trim, got {:?}",
        ranges[0]
    );
}

#[async_test]
async fn trim_then_write_reallocates(driver: DefaultDriver) {
    let vhdx = create_and_write_block(format::GB1, 0, &driver).await;

    // Trim with FileSpace (soft anchor).
    vhdx.trim(TrimRequest::new(
        TrimMode::FileSpace,
        0,
        vhdx.block_size() as u64,
    ))
    .await
    .unwrap();
    assert_block_state(&vhdx, 0, BatEntryState::Unmapped);

    // Write again — should reallocate (possibly reusing soft anchor).
    let bs = vhdx.block_size();
    let mut ranges = Vec::new();
    let guard = vhdx.resolve_write(0, bs, &mut ranges).await.unwrap();
    write_ranges(&vhdx, &ranges, 0xFF).await;
    guard.complete().await.unwrap();

    assert_block_state(&vhdx, 0, BatEntryState::FullyPresent);
}

// ---- Fully-Allocated Disk Tests ----

#[async_test]
async fn trim_fixed_disk_file_space_noop(driver: DefaultDriver) {
    let file = InMemoryFile::new(0);
    let mut params = CreateParams {
        disk_size: 4 * MB1,
        block_size: MB1 as u32,
        is_fully_allocated: true,
        ..Default::default()
    };
    create::create(&file, &mut params).await.unwrap();
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    // FileSpace trim on fixed → no-op.
    vhdx.trim(TrimRequest::new(TrimMode::FileSpace, 0, 4 * MB1))
        .await
        .unwrap();

    // Blocks should be unchanged.
    let mapping = vhdx.bat.get_block_mapping(0);
    let state = mapping.bat_state();
    // On a fully-allocated disk, blocks start as Undefined (not yet written).
    // The FileSpace mode is a no-op, so they stay the same.
    assert_ne!(state, BatEntryState::Unmapped);
}

#[async_test]
async fn trim_fixed_disk_make_transparent_allowed(driver: DefaultDriver) {
    let file = InMemoryFile::new(0);
    let mut params = CreateParams {
        disk_size: 4 * MB1,
        block_size: MB1 as u32,
        is_fully_allocated: true,
        ..Default::default()
    };
    create::create(&file, &mut params).await.unwrap();
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    // MakeTransparent on fixed → allowed.
    vhdx.trim(TrimRequest::new(TrimMode::MakeTransparent, 0, 4 * MB1))
        .await
        .unwrap();

    // Blocks should be NotPresent (MakeTransparent succeeded).
    assert_block_state(&vhdx, 0, BatEntryState::NotPresent);
}

// ---- Concurrent Safety Tests ----

#[async_test]
async fn trim_waits_for_in_flight_read(driver: DefaultDriver) {
    let vhdx = create_and_write_block(format::GB1, 0, &driver).await;

    // Acquire a read guard on block 0 to hold its refcount.
    let mut ranges = Vec::new();
    let read_guard = vhdx.resolve_read(0, 4096, &mut ranges).await.unwrap();

    // Spawn trim concurrently. It should block until the guard is dropped.
    let trim_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let trim_done2 = trim_done.clone();

    let (trim_result, _) = futures::join!(
        async {
            let r = vhdx
                .trim(TrimRequest::new(
                    TrimMode::FileSpace,
                    0,
                    vhdx.block_size() as u64,
                ))
                .await;
            trim_done2.store(true, std::sync::atomic::Ordering::SeqCst);
            r
        },
        async {
            // After a yield, drop the read guard.
            // The trim should be able to see the refcount eventually.
            // Yield to let the trim task run.
            std::future::poll_fn(|cx| {
                cx.waker().wake_by_ref();
                std::task::Poll::Ready(())
            })
            .await;
            assert!(
                !trim_done.load(std::sync::atomic::Ordering::SeqCst),
                "trim should not complete while read guard is held"
            );
            drop(read_guard);
        }
    );

    trim_result.unwrap();
    assert_block_state(&vhdx, 0, BatEntryState::Unmapped);
}

#[async_test]
async fn trim_waits_for_in_flight_write(driver: DefaultDriver) {
    let vhdx = create_and_write_block(format::GB1, 0, &driver).await;

    // Acquire a write guard on block 0.
    let mut ranges = Vec::new();
    let write_guard = vhdx
        .resolve_write(0, vhdx.block_size(), &mut ranges)
        .await
        .unwrap();

    let trim_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let trim_done2 = trim_done.clone();

    let (trim_result, _) = futures::join!(
        async {
            let r = vhdx
                .trim(TrimRequest::new(
                    TrimMode::FileSpace,
                    0,
                    vhdx.block_size() as u64,
                ))
                .await;
            trim_done2.store(true, std::sync::atomic::Ordering::SeqCst);
            r
        },
        async {
            // Yield to let the trim task run.
            std::future::poll_fn(|cx| {
                cx.waker().wake_by_ref();
                std::task::Poll::Ready(())
            })
            .await;
            assert!(
                !trim_done.load(std::sync::atomic::Ordering::SeqCst),
                "trim should not complete while write guard is held"
            );
            // Complete the write so the guard drops after.
            write_guard.complete().await.unwrap();
        }
    );

    trim_result.unwrap();
    assert_block_state(&vhdx, 0, BatEntryState::Unmapped);
}

#[async_test]
async fn trim_concurrent_different_block(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let bs = vhdx.block_size();

    // Write blocks 0 and 1.
    for block in 0..2u32 {
        let offset = block as u64 * bs as u64;
        let mut ranges = Vec::new();
        let guard = vhdx.resolve_write(offset, bs, &mut ranges).await.unwrap();
        write_ranges(&vhdx, &ranges, 0xAA).await;
        guard.complete().await.unwrap();
    }

    // Trim block 0, read block 1 concurrently.
    let (trim_result, read_result) = futures::join!(
        vhdx.trim(TrimRequest::new(TrimMode::FileSpace, 0, bs as u64)),
        async {
            let mut ranges = Vec::new();
            let guard = vhdx
                .resolve_read(bs as u64, 4096, &mut ranges)
                .await
                .unwrap();
            let result = ranges.clone();
            drop(guard);
            result
        }
    );

    trim_result.unwrap();
    assert_block_state(&vhdx, 0, BatEntryState::Unmapped);
    assert!(matches!(read_result[0], ReadRange::Data { .. }));
}

// ---- Validation Tests ----

#[async_test]
async fn trim_read_only_fails() {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).read_only().await.unwrap();

    let result = vhdx
        .trim(TrimRequest::new(
            TrimMode::FileSpace,
            0,
            vhdx.block_size() as u64,
        ))
        .await;
    assert!(matches!(
        result,
        Err(VhdxIoError(VhdxIoErrorInner::ReadOnly))
    ));
}

#[async_test]
async fn trim_unaligned_offset_fails(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    let result = vhdx
        .trim(TrimRequest::new(TrimMode::FileSpace, 1, 512))
        .await;
    assert!(matches!(
        result,
        Err(VhdxIoError(VhdxIoErrorInner::UnalignedIo))
    ));
}

#[async_test]
async fn trim_beyond_disk_fails(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    let result = vhdx
        .trim(TrimRequest::new(
            TrimMode::FileSpace,
            format::GB1 - 512,
            1024,
        ))
        .await;
    assert!(matches!(
        result,
        Err(VhdxIoError(VhdxIoErrorInner::BeyondEndOfDisk))
    ));
}

#[async_test]
async fn trim_beyond_disk_ok_with_skip(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    // With skip_disk_size_check, goes beyond but computes no included blocks → ok.
    let result = vhdx
        .trim(
            TrimRequest::new(TrimMode::FileSpace, format::GB1 - 512, 1024)
                .skip_disk_size_check(true),
        )
        .await;
    assert!(result.is_ok());
}

#[async_test]
async fn trim_zero_length_noop(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    let result = vhdx.trim(TrimRequest::new(TrimMode::FileSpace, 0, 0)).await;
    assert!(result.is_ok());
}
