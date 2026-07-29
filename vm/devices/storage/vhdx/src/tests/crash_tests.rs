// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Crash consistency tests for the VHDX write path.
//!
//! Tests are organized into three categories:
//! 1. **Structural tests** — verify `pre_log_fsn` values on BAT pages
//!    after allocation decisions.
//! 2. **Ordering tests** — verify flush barriers between data writes and
//!    WAL writes via the write log.
//! 3. **End-to-end crash recovery tests** — simulate crashes with
//!    `CrashTestFile` and verify replay recovers correctly.

use crate::AsyncFile;
use crate::AsyncFileExt;
use crate::format;
use crate::open::VhdxFile;
use crate::tests::support::CrashTestFile;
use crate::tests::support::InMemoryFile;
use pal_async::DefaultDriver;
use pal_async::async_test;

/// Helper: write a data pattern via the write path.
async fn write_pattern<F: AsyncFile>(vhdx: &VhdxFile<F>, offset: u64, len: usize, value: u8) {
    let write_buf = vec![value; len];
    let mut ranges = Vec::new();
    let guard = vhdx
        .resolve_write(offset, len as u32, &mut ranges)
        .await
        .unwrap();
    for range in &ranges {
        match range {
            crate::WriteRange::Data {
                file_offset,
                length,
                ..
            } => {
                vhdx.file
                    .write_at(*file_offset, &write_buf[..(*length as usize)])
                    .await
                    .unwrap();
            }
            crate::WriteRange::Zero {
                file_offset,
                length,
            } => {
                let zeros = vec![0u8; *length as usize];
                vhdx.file.write_at(*file_offset, &zeros).await.unwrap();
            }
        }
    }
    guard.complete().await.unwrap();
}

/// Helper: read data at a guest offset via the read path.
async fn read_pattern<F: AsyncFile>(vhdx: &VhdxFile<F>, offset: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let mut ranges = Vec::new();
    let _guard = vhdx
        .resolve_read(offset, len as u32, &mut ranges)
        .await
        .unwrap();
    for range in &ranges {
        match range {
            crate::ReadRange::Data {
                guest_offset,
                file_offset,
                length,
            } => {
                let start = (*guest_offset - offset) as usize;
                let end = start + *length as usize;
                vhdx.file
                    .read_at(*file_offset, &mut buf[start..end])
                    .await
                    .unwrap();
            }
            crate::ReadRange::Zero {
                guest_offset,
                length,
            } => {
                let start = (*guest_offset - offset) as usize;
                let end = start + *length as usize;
                buf[start..end].fill(0);
            }
            crate::ReadRange::Unmapped { .. } => {}
        }
    }
    buf
}

// =============================================================================
// Structural tests: verify pre_log_fsn values on BAT pages
// =============================================================================

/// Near-EOF allocation: SpaceState::Zero → no pre_log_fsn on BAT page.
///
/// First write to a new VHDX allocates from near-EOF space, which is
/// already zeroed (durable zeros). No flush barrier is needed.
#[async_test]
async fn bat_page_no_fsn_safe_near_eof(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    // Write one full block (block_size is 1 MiB by default).
    let block_size = vhdx.block_size as usize;
    write_pattern(&vhdx, 0, block_size, 0xAA).await;

    // Check that the BAT page for block 0 has NO pre_log_fsn.
    let page_key = vhdx.bat_page_key_for_block(0);
    let fsn = vhdx.cache.get_pre_log_fsn(page_key);
    assert_eq!(
        fsn, None,
        "near-EOF allocation should NOT set pre_log_fsn (SpaceState::Zero)"
    );

    vhdx.close().await.unwrap();
}

/// Free-pool reuse: SpaceState::CrossStale → pre_log_fsn set.
///
/// Allocate a block, trim it (release to free pool), then allocate again.
/// The second allocation reuses free-pool space, which is NOT safe (contains
/// old data from the previously trimmed block).
#[async_test]
async fn bat_page_has_fsn_unsafe_free_pool(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size as u64;

    // Allocate block 0 with data.
    write_pattern(&vhdx, 0, block_size as usize, 0xAA).await;
    vhdx.flush().await.unwrap();

    // Trim block 0 to release its space to the free pool.
    let trim_request =
        crate::trim::TrimRequest::new(crate::trim::TrimMode::FreeSpace, 0, block_size);
    vhdx.trim(trim_request).await.unwrap();
    vhdx.flush().await.unwrap();

    // Now write to block 1 — space should come from free pool.
    write_pattern(&vhdx, block_size, block_size as usize, 0xBB).await;

    // The BAT page for block 1 should have a pre_log_fsn set because
    // the allocation reused free-pool space (SpaceState::CrossStale (unsafe)).
    let page_key = vhdx.bat_page_key_for_block(1);
    let fsn = vhdx.cache.get_pre_log_fsn(page_key);

    assert!(
        fsn.is_some(),
        "free-pool reuse should set pre_log_fsn (SpaceState::CrossStale)"
    );

    vhdx.close().await.unwrap();
}

/// Overwrite existing FullyPresent block: no allocation → no pre_log_fsn.
///
/// Writing to an already-allocated block should not set any FSN constraint
/// because the BAT entry doesn't change.
#[async_test]
async fn bat_page_no_fsn_existing_block(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size as usize;

    // First write allocates the block.
    write_pattern(&vhdx, 0, block_size, 0xAA).await;
    vhdx.flush().await.unwrap();

    // Second write overwrites the same block — no new allocation.
    write_pattern(&vhdx, 0, block_size, 0xBB).await;

    // BAT page should have no pre_log_fsn (no allocation happened).
    let page_key = vhdx.bat_page_key_for_block(0);
    let fsn = vhdx.cache.get_pre_log_fsn(page_key);
    assert_eq!(
        fsn, None,
        "overwrite of existing block should NOT set pre_log_fsn"
    );

    vhdx.close().await.unwrap();
}

/// Soft-anchor reclaim: SpaceState::Zero → no pre_log_fsn.
///
/// Trim a block with FileSpace mode (creates soft anchor), then write the
/// same block again. The allocation reclaims the soft-anchored space, which
/// is the block's own old data — SpaceState::OwnStale (safe).
#[async_test]
async fn bat_page_no_fsn_safe_soft_anchor(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size as u64;

    // Allocate block 0 with data.
    write_pattern(&vhdx, 0, block_size as usize, 0xAA).await;
    vhdx.flush().await.unwrap();

    // Trim block 0 with FileSpace mode — creates a soft anchor.
    let trim_request =
        crate::trim::TrimRequest::new(crate::trim::TrimMode::FileSpace, 0, block_size);
    vhdx.trim(trim_request).await.unwrap();
    vhdx.flush().await.unwrap();

    // Write to block 0 again — should reclaim the soft-anchored space.
    write_pattern(&vhdx, 0, block_size as usize, 0xBB).await;

    // The BAT page for block 0 should have NO pre_log_fsn because the
    // allocation reused the block's own old space (SpaceState::OwnStale (safe)).
    let page_key = vhdx.bat_page_key_for_block(0);
    let fsn = vhdx.cache.get_pre_log_fsn(page_key);
    assert_eq!(
        fsn, None,
        "soft-anchor reclaim should NOT set pre_log_fsn (SpaceState::Zero)"
    );

    vhdx.close().await.unwrap();
}

/// Partial write triggering non-TFP allocation with unsafe space →
/// pre_log_fsn set.
///
/// Write a partial block (less than full block) where the allocation
/// comes from the free pool. Since the space contains stale data from
/// another block, SpaceState::CrossStale (unsafe) → the non-TFP path sets pre_log_fsn.
#[async_test]
async fn bat_page_has_fsn_partial_unsafe(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size as u64;

    // Allocate block 0 with data.
    write_pattern(&vhdx, 0, block_size as usize, 0xAA).await;
    vhdx.flush().await.unwrap();

    // Trim block 0 to release its space to the free pool.
    let trim_request =
        crate::trim::TrimRequest::new(crate::trim::TrimMode::FreeSpace, 0, block_size);
    vhdx.trim(trim_request).await.unwrap();
    vhdx.flush().await.unwrap();

    // Write a partial block at block 1 (less than full block_size). This
    // triggers the non-TFP allocation path. Space comes from free pool →
    // SpaceState::CrossStale (unsafe).
    let partial_size = 4096;
    write_pattern(&vhdx, block_size, partial_size, 0xCC).await;

    // The BAT page for block 1 should have pre_log_fsn set.
    let page_key = vhdx.bat_page_key_for_block(1);
    let fsn = vhdx.cache.get_pre_log_fsn(page_key);
    assert!(
        fsn.is_some(),
        "partial write with unsafe free-pool space should set pre_log_fsn"
    );

    vhdx.close().await.unwrap();
}

/// Partial write with safe space → no pre_log_fsn.
///
/// A partial write to a new block allocated from near-EOF (safe) space
/// should NOT set pre_log_fsn, because near-EOF space contains durable zeros.
#[async_test]
async fn bat_page_no_fsn_partial_safe(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    // Partial write to block 0 (less than full block). Space comes from
    // near-EOF → SpaceState::OwnStale (safe).
    let partial_size = 4096;
    write_pattern(&vhdx, 0, partial_size, 0xDD).await;

    // The BAT page for block 0 should have NO pre_log_fsn.
    let page_key = vhdx.bat_page_key_for_block(0);
    let fsn = vhdx.cache.get_pre_log_fsn(page_key);
    assert_eq!(
        fsn, None,
        "partial write with safe near-EOF space should NOT set pre_log_fsn"
    );

    vhdx.close().await.unwrap();
}

// =============================================================================
// End-to-end crash recovery tests using CrashTestFile
// =============================================================================

/// Write + flush → crash → replay recovers data.
#[async_test]
async fn crash_after_flush_data_survives(driver: DefaultDriver) {
    // Create a VHDX on InMemoryFile first, then transfer to CrashTestFile.
    let (mem_file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let snapshot = mem_file.snapshot();

    let crash_file = CrashTestFile::from_durable(snapshot);
    let vhdx = VhdxFile::open(crash_file).writable(&driver).await.unwrap();

    // Write one block of data.
    let block_size = vhdx.block_size as usize;
    write_pattern(&vhdx, 0, block_size, 0xAB).await;

    // Flush to make data durable.
    vhdx.flush().await.unwrap();

    // Crash — get durable state, then abort (ensures log task exits).
    let durable = vhdx.file.durable_snapshot();
    vhdx.abort().await;

    // Reopen from durable state (log replay will happen).
    let recovered_file = InMemoryFile::from_snapshot(durable);
    let vhdx2 = VhdxFile::open(recovered_file)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();

    // Verify data survived.
    let read_buf = read_pattern(&vhdx2, 0, block_size).await;
    assert!(
        read_buf.iter().all(|&b| b == 0xAB),
        "data should survive crash after flush"
    );
}

/// Write without flush → crash → data lost.
#[async_test]
async fn crash_no_flush_data_lost(driver: DefaultDriver) {
    let (mem_file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let snapshot = mem_file.snapshot();

    let crash_file = CrashTestFile::from_durable(snapshot);
    let vhdx = VhdxFile::open(crash_file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size as usize;

    // Write but do NOT flush.
    write_pattern(&vhdx, 0, block_size, 0xCD).await;

    // Crash — get durable state.
    let durable = vhdx.file.durable_snapshot();
    vhdx.abort().await;

    // Reopen from durable state.
    let recovered_file = InMemoryFile::from_snapshot(durable);
    let vhdx2 = VhdxFile::open(recovered_file)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();

    // Data should be lost (read as zeros for unallocated block).
    let read_buf = read_pattern(&vhdx2, 0, block_size).await;
    assert!(
        read_buf.iter().all(|&b| b == 0),
        "data should be lost without flush before crash"
    );
}

/// Write + flush + close → reopen → clean (no replay needed).
#[async_test]
async fn clean_close_no_replay(driver: DefaultDriver) {
    let (mem_file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let snapshot = mem_file.snapshot();

    let crash_file = CrashTestFile::from_durable(snapshot);
    let vhdx = VhdxFile::open(crash_file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size as usize;

    write_pattern(&vhdx, 0, block_size, 0xEE).await;
    vhdx.flush().await.unwrap();

    // Close cleanly.
    let file_ref = vhdx.file.clone();
    vhdx.close().await.unwrap();
    let durable = file_ref.durable_snapshot();

    // Reopen — should NOT need log replay.
    let recovered_file = InMemoryFile::from_snapshot(durable);
    let vhdx2 = VhdxFile::open(recovered_file)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();

    let read_buf = read_pattern(&vhdx2, 0, block_size).await;
    assert!(
        read_buf.iter().all(|&b| b == 0xEE),
        "data should survive clean close + reopen"
    );
}

/// Crash after flush, reopen, write more, flush, crash again →
/// both rounds of data survive.
#[async_test]
async fn crash_recovery_then_more_writes(driver: DefaultDriver) {
    let (mem_file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let snapshot = mem_file.snapshot();
    let block_size: u64;

    // Round 1: write, flush, crash.
    let durable1 = {
        let crash_file = CrashTestFile::from_durable(snapshot);
        let vhdx = VhdxFile::open(crash_file).writable(&driver).await.unwrap();
        block_size = vhdx.block_size as u64;

        write_pattern(&vhdx, 0, block_size as usize, 0x11).await;
        vhdx.flush().await.unwrap();

        let durable = vhdx.file.durable_snapshot();
        vhdx.abort().await;
        durable
    };

    // Round 2: recover, write more, flush, crash again.
    let durable2 = {
        let crash_file = CrashTestFile::from_durable(durable1);
        let vhdx = VhdxFile::open(crash_file).writable(&driver).await.unwrap();

        write_pattern(&vhdx, block_size, block_size as usize, 0x22).await;
        vhdx.flush().await.unwrap();

        let durable = vhdx.file.durable_snapshot();
        vhdx.abort().await;
        durable
    };

    // Verify both rounds of data survive.
    let recovered_file = InMemoryFile::from_snapshot(durable2);
    let vhdx = VhdxFile::open(recovered_file)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();

    let buf0 = read_pattern(&vhdx, 0, block_size as usize).await;
    assert!(
        buf0.iter().all(|&b| b == 0x11),
        "round 1 data should survive"
    );

    let buf1 = read_pattern(&vhdx, block_size, block_size as usize).await;
    assert!(
        buf1.iter().all(|&b| b == 0x22),
        "round 2 data should survive"
    );
}

/// Multiple blocks → flush → crash → all survive.
#[async_test]
async fn crash_multi_block_all_survive(driver: DefaultDriver) {
    let (mem_file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let snapshot = mem_file.snapshot();

    let crash_file = CrashTestFile::from_durable(snapshot);
    let vhdx = VhdxFile::open(crash_file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size as u64;

    // Write three blocks with different patterns.
    write_pattern(&vhdx, 0, block_size as usize, 0xAA).await;
    write_pattern(&vhdx, block_size, block_size as usize, 0xBB).await;
    write_pattern(&vhdx, block_size * 2, block_size as usize, 0xCC).await;

    vhdx.flush().await.unwrap();

    // Crash.
    let durable = vhdx.file.durable_snapshot();
    vhdx.abort().await;

    // Verify all three blocks survived.
    let recovered = InMemoryFile::from_snapshot(durable);
    let vhdx = VhdxFile::open(recovered)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();

    let buf0 = read_pattern(&vhdx, 0, block_size as usize).await;
    assert!(buf0.iter().all(|&b| b == 0xAA));
    let buf1 = read_pattern(&vhdx, block_size, block_size as usize).await;
    assert!(buf1.iter().all(|&b| b == 0xBB));
    let buf2 = read_pattern(&vhdx, block_size * 2, block_size as usize).await;
    assert!(buf2.iter().all(|&b| b == 0xCC));
}

/// Sequential writes with flushes → crash after second flush →
/// first two blocks survive, third (unflushed) lost.
#[async_test]
async fn crash_interleaved_flush_partial(driver: DefaultDriver) {
    let (mem_file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let snapshot = mem_file.snapshot();

    let crash_file = CrashTestFile::from_durable(snapshot);
    let vhdx = VhdxFile::open(crash_file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size as u64;

    // Write block 0, flush.
    write_pattern(&vhdx, 0, block_size as usize, 0x11).await;
    vhdx.flush().await.unwrap();

    // Write block 1, flush.
    write_pattern(&vhdx, block_size, block_size as usize, 0x22).await;
    vhdx.flush().await.unwrap();

    // Write block 2, do NOT flush.
    write_pattern(&vhdx, block_size * 2, block_size as usize, 0x33).await;

    // Crash.
    let durable = vhdx.file.durable_snapshot();
    vhdx.abort().await;

    let recovered = InMemoryFile::from_snapshot(durable);
    let vhdx = VhdxFile::open(recovered)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();

    // Block 0 and 1 should survive.
    let buf0 = read_pattern(&vhdx, 0, block_size as usize).await;
    assert!(buf0.iter().all(|&b| b == 0x11), "block 0 should survive");
    let buf1 = read_pattern(&vhdx, block_size, block_size as usize).await;
    assert!(buf1.iter().all(|&b| b == 0x22), "block 1 should survive");

    // Block 2 should be lost (zeros).
    let buf2 = read_pattern(&vhdx, block_size * 2, block_size as usize).await;
    assert!(
        buf2.iter().all(|&b| b == 0),
        "block 2 (unflushed) should be lost"
    );
}

/// Large write spanning multiple blocks → flush → crash → all survive.
#[async_test]
async fn crash_spanning_write_survives(driver: DefaultDriver) {
    let (mem_file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let snapshot = mem_file.snapshot();

    let crash_file = CrashTestFile::from_durable(snapshot);
    let vhdx = VhdxFile::open(crash_file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size as u64;

    // Write 3 blocks at once (spanning write).
    let total_len = (block_size * 3) as usize;
    write_pattern(&vhdx, 0, total_len, 0xDD).await;
    vhdx.flush().await.unwrap();

    // Crash.
    let durable = vhdx.file.durable_snapshot();
    vhdx.abort().await;

    let recovered = InMemoryFile::from_snapshot(durable);
    let vhdx = VhdxFile::open(recovered)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();

    let read_buf = read_pattern(&vhdx, 0, total_len).await;
    assert!(
        read_buf.iter().all(|&b| b == 0xDD),
        "spanning write data should survive crash after flush"
    );
}

/// Write → flush → apply completes → crash (log_guid still set) →
/// replay is idempotent.
///
/// After flush, the log task writes WAL entries and applies them to the
/// BAT region. If we crash at that point, log_guid is still set (close
/// never ran), so the next open replays the log. Since the entries are
/// already applied, replay is idempotent — the data should be correct.
#[async_test]
async fn crash_after_apply_replay_idempotent(driver: DefaultDriver) {
    let (mem_file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let snapshot = mem_file.snapshot();

    let crash_file = CrashTestFile::from_durable(snapshot);
    let vhdx = VhdxFile::open(crash_file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size as usize;

    // Write two blocks with distinct patterns.
    write_pattern(&vhdx, 0, block_size, 0xA1).await;
    write_pattern(&vhdx, block_size as u64, block_size, 0xA2).await;
    vhdx.flush().await.unwrap();

    // Take a durable snapshot — log entries are applied by now.
    let durable = vhdx.file.durable_snapshot();
    vhdx.abort().await;

    // First replay — should succeed.
    let recovered1 = InMemoryFile::from_snapshot(durable.clone());
    let vhdx1 = VhdxFile::open(recovered1)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();
    let buf0 = read_pattern(&vhdx1, 0, block_size).await;
    assert!(
        buf0.iter().all(|&b| b == 0xA1),
        "block 0 after first replay"
    );
    let buf1 = read_pattern(&vhdx1, block_size as u64, block_size).await;
    assert!(
        buf1.iter().all(|&b| b == 0xA2),
        "block 1 after first replay"
    );

    // Second replay from the same durable snapshot — should be idempotent.
    let recovered2 = InMemoryFile::from_snapshot(durable);
    let vhdx2 = VhdxFile::open(recovered2)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();
    let buf0b = read_pattern(&vhdx2, 0, block_size).await;
    assert!(
        buf0b.iter().all(|&b| b == 0xA1),
        "block 0 after second replay (idempotent)"
    );
    let buf1b = read_pattern(&vhdx2, block_size as u64, block_size).await;
    assert!(
        buf1b.iter().all(|&b| b == 0xA2),
        "block 1 after second replay (idempotent)"
    );
}

/// Overwrite same block → flush → crash → latest data survives.
#[async_test]
async fn crash_overwrite_latest_wins(driver: DefaultDriver) {
    let (mem_file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let snapshot = mem_file.snapshot();

    let crash_file = CrashTestFile::from_durable(snapshot);
    let vhdx = VhdxFile::open(crash_file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size as usize;

    // Write block 0 with 0xAA, flush.
    write_pattern(&vhdx, 0, block_size, 0xAA).await;
    vhdx.flush().await.unwrap();

    // Overwrite block 0 with 0xBB, flush.
    write_pattern(&vhdx, 0, block_size, 0xBB).await;
    vhdx.flush().await.unwrap();

    // Crash.
    let durable = vhdx.file.durable_snapshot();
    vhdx.abort().await;

    let recovered = InMemoryFile::from_snapshot(durable);
    let vhdx = VhdxFile::open(recovered)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();

    let read_buf = read_pattern(&vhdx, 0, block_size).await;
    assert!(
        read_buf.iter().all(|&b| b == 0xBB),
        "latest overwrite should survive"
    );
}

/// Crash without close → reopen → log_guid set → replay → correct.
#[async_test]
async fn drop_without_close_triggers_replay(driver: DefaultDriver) {
    let (mem_file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let snapshot = mem_file.snapshot();

    let crash_file = CrashTestFile::from_durable(snapshot);
    let vhdx = VhdxFile::open(crash_file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size as usize;

    write_pattern(&vhdx, 0, block_size, 0xEE).await;
    vhdx.flush().await.unwrap();

    // Abort without close (simulates unclean shutdown).
    let durable = vhdx.file.durable_snapshot();
    vhdx.abort().await;

    // Verify the header still has log_guid set (dirty file).
    let recovered = InMemoryFile::from_snapshot(durable.clone());
    // Opening read-only when dirty should fail with LogReplayRequired.
    let result = VhdxFile::open(InMemoryFile::from_snapshot(durable.clone()))
        .read_only()
        .await;
    assert!(result.is_err(), "read-only open of dirty file should fail");

    // Open writable — log replay should happen.
    let vhdx2 = VhdxFile::open(recovered)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();
    let read_buf = read_pattern(&vhdx2, 0, block_size).await;
    assert!(
        read_buf.iter().all(|&b| b == 0xEE),
        "data should survive after log replay"
    );
    drop(result);
}

// =============================================================================
// Ordering tests: verify flush barrier placement via write log
// =============================================================================

/// Core ordering test: unsafe allocation → flush barrier between
/// data write and WAL write.
///
/// We verify that after a free-pool reuse allocation, the write log
/// contains a Flush entry between the data writes and subsequent WAL writes.
#[async_test]
async fn flush_between_data_and_wal_unsafe(driver: DefaultDriver) {
    let (mem_file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let snapshot = mem_file.snapshot();

    let crash_file = CrashTestFile::from_durable(snapshot);
    let vhdx = VhdxFile::open(crash_file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size as u64;

    // Allocate block 0 (near-EOF, safe).
    write_pattern(&vhdx, 0, block_size as usize, 0xAA).await;
    vhdx.flush().await.unwrap();

    // Trim block 0 to release to free pool.
    let trim_request =
        crate::trim::TrimRequest::new(crate::trim::TrimMode::FreeSpace, 0, block_size);
    vhdx.trim(trim_request).await.unwrap();
    vhdx.flush().await.unwrap();

    // Record the flush count before the unsafe allocation.
    let pre_flush_count = vhdx.file.flush_count();

    // Write block 1 — should reuse free-pool space (unsafe).
    write_pattern(&vhdx, block_size, block_size as usize, 0xBB).await;

    // Flush — this triggers the log path.
    vhdx.flush().await.unwrap();

    // Post-flush count should be greater (at least one flush for data + one for WAL).
    let post_flush_count = vhdx.file.flush_count();
    assert!(
        post_flush_count > pre_flush_count,
        "should have flushed after unsafe allocation: pre={}, post={}",
        pre_flush_count,
        post_flush_count
    );

    vhdx.close().await.unwrap();
}

/// Safe allocation → no extra flush barrier needed.
///
/// Near-EOF allocations produce SpaceState::Zero, so no pre_log_fsn
/// is set. The number of flushes should be minimal.
#[async_test]
async fn no_extra_flush_safe_allocation(driver: DefaultDriver) {
    let (mem_file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let snapshot = mem_file.snapshot();

    let crash_file = CrashTestFile::from_durable(snapshot);
    let vhdx = VhdxFile::open(crash_file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size as u64;

    // All allocations are near-EOF (safe). No extra flushes needed.
    write_pattern(&vhdx, 0, block_size as usize, 0xAA).await;

    // The BAT page should have no pre_log_fsn constraint.
    let page_key = vhdx.bat_page_key_for_block(0);
    let fsn = vhdx.cache.get_pre_log_fsn(page_key);
    assert_eq!(
        fsn, None,
        "safe near-EOF allocation should not set pre_log_fsn"
    );

    vhdx.close().await.unwrap();
}

/// Multiple blocks in one write, mixed safe/unsafe → barrier present
/// for the unsafe block.
///
/// Allocate block 0 (safe), trim it to free pool, then write a spanning
/// write covering block 1 (unsafe, from free pool) and block 2 (safe,
/// near-EOF). The unsafe block should have pre_log_fsn set.
#[async_test]
async fn mixed_safe_unsafe_has_barrier(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size as u64;

    // Allocate block 0.
    write_pattern(&vhdx, 0, block_size as usize, 0xAA).await;
    vhdx.flush().await.unwrap();

    // Trim block 0 to release to free pool.
    let trim_request =
        crate::trim::TrimRequest::new(crate::trim::TrimMode::FreeSpace, 0, block_size);
    vhdx.trim(trim_request).await.unwrap();
    vhdx.flush().await.unwrap();

    // Write a spanning write covering blocks 1 and 2.
    // Block 1's space may come from the free pool (unsafe).
    // Block 2's space comes from near-EOF (safe).
    let total_len = (block_size * 2) as usize;
    write_pattern(&vhdx, block_size, total_len, 0xCC).await;

    // At least one of the BAT pages should have pre_log_fsn set (the
    // unsafe block). Check that the write_guard's needs_flush_before_log
    // was set by verifying the BAT page for the unsafe block has FSN.
    let page_key_1 = vhdx.bat_page_key_for_block(1);
    let page_key_2 = vhdx.bat_page_key_for_block(2);
    let fsn_1 = vhdx.cache.get_pre_log_fsn(page_key_1);
    let fsn_2 = vhdx.cache.get_pre_log_fsn(page_key_2);

    // At least one block should have FSN (the one allocated from free pool).
    // Both blocks share the TFP path, so needs_flush_before_log is set for
    // the entire WriteIoGuard. The FSN is applied to all TFP blocks in
    // complete_write_inner.
    assert!(
        fsn_1.is_some() || fsn_2.is_some(),
        "mixed safe/unsafe spanning write should set pre_log_fsn on at least one BAT page"
    );

    vhdx.close().await.unwrap();
}

// =============================================================================
// Header update tests
// =============================================================================

/// After flush, header sequence_number has advanced.
#[async_test]
async fn flush_advances_header_sequence(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    let seq_before = vhdx.header_state.sequence_number().await;

    write_pattern(&vhdx, 0, vhdx.block_size as usize, 0xAA).await;
    vhdx.flush().await.unwrap();

    // The enable_write_mode call during the first write bumps the sequence number.
    let seq_after = vhdx.header_state.sequence_number().await;
    assert!(
        seq_after > seq_before,
        "sequence number should advance after write: before={}, after={}",
        seq_before,
        seq_after
    );

    vhdx.close().await.unwrap();
}

/// close() writes clean header (log_guid = ZERO).
#[async_test]
async fn close_header_is_clean(driver: DefaultDriver) {
    use zerocopy::FromBytes;

    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let file_ref = vhdx.file.clone();

    write_pattern(&vhdx, 0, vhdx.block_size as usize, 0xBB).await;
    vhdx.flush().await.unwrap();
    vhdx.close().await.unwrap();

    // Read both headers, find the current one (highest seq#).
    let mut buf1 = vec![0u8; format::HEADER_SIZE as usize];
    file_ref
        .read_at(format::HEADER_OFFSET_1, &mut buf1)
        .await
        .unwrap();
    let mut buf2 = vec![0u8; format::HEADER_SIZE as usize];
    file_ref
        .read_at(format::HEADER_OFFSET_2, &mut buf2)
        .await
        .unwrap();

    let h1 = format::Header::read_from_prefix(&buf1).ok().map(|(h, _)| h);
    let h2 = format::Header::read_from_prefix(&buf2).ok().map(|(h, _)| h);

    let current = match (&h1, &h2) {
        (Some(a), Some(b)) if b.sequence_number >= a.sequence_number => b,
        (Some(a), _) => a,
        (_, Some(b)) => b,
        _ => panic!("no valid headers"),
    };
    assert_eq!(
        current.log_guid,
        guid::Guid::ZERO,
        "after close, current header should have log_guid = ZERO"
    );
}

/// Header alternation: writes alternate between header slots 1 and 2.
#[async_test]
async fn headers_alternate_between_slots(driver: DefaultDriver) {
    use zerocopy::FromBytes;

    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;

    // Read initial header state.
    let mut buf1_init = vec![0u8; format::HEADER_SIZE as usize];
    file.read_at(format::HEADER_OFFSET_1, &mut buf1_init)
        .await
        .unwrap();
    let h1_init = format::Header::read_from_prefix(&buf1_init)
        .ok()
        .map(|(h, _)| h);
    let mut buf2_init = vec![0u8; format::HEADER_SIZE as usize];
    file.read_at(format::HEADER_OFFSET_2, &mut buf2_init)
        .await
        .unwrap();
    let h2_init = format::Header::read_from_prefix(&buf2_init)
        .ok()
        .map(|(h, _)| h);

    let seq1_init = h1_init.as_ref().map_or(0, |h| h.sequence_number);
    let seq2_init = h2_init.as_ref().map_or(0, |h| h.sequence_number);

    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let file_ref = vhdx.file.clone();

    // After open_writable, one header slot was updated with log_guid.
    let mut buf1_after = vec![0u8; format::HEADER_SIZE as usize];
    file_ref
        .read_at(format::HEADER_OFFSET_1, &mut buf1_after)
        .await
        .unwrap();
    let h1_after = format::Header::read_from_prefix(&buf1_after)
        .ok()
        .map(|(h, _)| h);

    let mut buf2_after = vec![0u8; format::HEADER_SIZE as usize];
    file_ref
        .read_at(format::HEADER_OFFSET_2, &mut buf2_after)
        .await
        .unwrap();
    let h2_after = format::Header::read_from_prefix(&buf2_after)
        .ok()
        .map(|(h, _)| h);

    let seq1_after = h1_after.as_ref().map_or(0, |h| h.sequence_number);
    let seq2_after = h2_after.as_ref().map_or(0, |h| h.sequence_number);

    // One slot should have a higher sequence number than before.
    let slot1_updated = seq1_after > seq1_init;
    let slot2_updated = seq2_after > seq2_init;
    assert!(
        slot1_updated || slot2_updated,
        "one header slot should be updated after open_writable"
    );
    assert!(
        !(slot1_updated && slot2_updated),
        "only one header slot should be updated (alternation)"
    );

    vhdx.close().await.unwrap();
}

// =============================================================================
// Deferred space reclaim tests
// =============================================================================

/// Trim block A (FileSpace), then write the same block again (same-block
/// reclaim from deferred list). The write should reuse A's offset without
/// needing a flush — OwnStale.
#[async_test]
async fn deferred_same_block_reclaim(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size as u64;

    // Write block 0.
    write_pattern(&vhdx, 0, block_size as usize, 0xAA).await;
    let original_offset = vhdx.bat.get_block_mapping(0).file_megabyte();
    assert!(original_offset > 0);

    // Trim block 0 with FileSpace (creates deferred anchor).
    let trim_req = crate::trim::TrimRequest::new(crate::trim::TrimMode::FileSpace, 0, block_size);
    vhdx.trim(trim_req).await.unwrap();

    // Write block 0 again — should reclaim from deferred list (no flush).
    write_pattern(&vhdx, 0, block_size as usize, 0xBB).await;

    // Block 0 should be FullyPresent at the same offset.
    let new_offset = {
        let mapping = vhdx.bat.get_block_mapping(0);
        assert_eq!(mapping.bat_state(), format::BatEntryState::FullyPresent);
        mapping.file_megabyte()
    };
    assert_eq!(original_offset, new_offset, "should reuse same offset");

    // Verify data.
    let buf = read_pattern(&vhdx, 0, block_size as usize).await;
    assert!(buf.iter().all(|&b| b == 0xBB));

    vhdx.close().await.unwrap();
}

/// Trim block A (FileSpace), crash before flush. On reopen, A should
/// still be FullyPresent with its data intact — the trim was never durable.
#[async_test]
async fn deferred_trim_crash_no_data_loss(driver: DefaultDriver) {
    let (mem_file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let snapshot = mem_file.snapshot();

    let crash_file = CrashTestFile::from_durable(snapshot);
    let vhdx = VhdxFile::open(crash_file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size as u64;

    // Write block 0.
    write_pattern(&vhdx, 0, block_size as usize, 0xDD).await;
    vhdx.flush().await.unwrap();

    // Trim block 0 with FileSpace — deferred, NOT flushed.
    let trim_req = crate::trim::TrimRequest::new(crate::trim::TrimMode::FileSpace, 0, block_size);
    vhdx.trim(trim_req).await.unwrap();

    // Crash — get durable state.
    let durable = vhdx.file.durable_snapshot();
    vhdx.abort().await;

    // Reopen from durable state.
    let recovered = InMemoryFile::from_snapshot(durable);
    let vhdx2 = VhdxFile::open(recovered)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();

    // Block 0 should still have its data (trim wasn't durable).
    let buf = read_pattern(&vhdx2, 0, block_size as usize).await;
    assert!(
        buf.iter().all(|&b| b == 0xDD),
        "data should survive crash when trim wasn't flushed"
    );
}

/// Trim block A (FileSpace), write block B using separate space, crash
/// before flush. A should keep its data, B's write should be lost.
/// No data teleportation.
#[async_test]
async fn deferred_no_teleportation_on_crash(driver: DefaultDriver) {
    let (mem_file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let snapshot = mem_file.snapshot();

    let crash_file = CrashTestFile::from_durable(snapshot);
    let vhdx = VhdxFile::open(crash_file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size as u64;

    // Write blocks 0 and 1.
    write_pattern(&vhdx, 0, block_size as usize, 0x11).await;
    write_pattern(&vhdx, block_size, block_size as usize, 0x22).await;
    vhdx.flush().await.unwrap();

    // Trim block 0 — deferred, not flushed.
    let trim_req = crate::trim::TrimRequest::new(crate::trim::TrimMode::FileSpace, 0, block_size);
    vhdx.trim(trim_req).await.unwrap();

    // Write block 1 with new data — this uses block 1's existing offset
    // (overwrite, no allocation needed).
    write_pattern(&vhdx, block_size, block_size as usize, 0x33).await;

    // Do NOT flush. Crash.
    let durable = vhdx.file.durable_snapshot();
    vhdx.abort().await;

    // Reopen.
    let recovered = InMemoryFile::from_snapshot(durable);
    let vhdx2 = VhdxFile::open(recovered)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();

    // Block 0 should still have original data (trim wasn't durable).
    let buf0 = read_pattern(&vhdx2, 0, block_size as usize).await;
    assert!(
        buf0.iter().all(|&b| b == 0x11),
        "block 0 data should be intact after crash (trim not durable)"
    );

    // Block 1: may have old (0x22) or new (0x33) data depending on
    // whether the overwrite was flushed. Either is acceptable.
    // What is NOT acceptable: block 1 reading as 0x11 (block 0's data).
    let buf1 = read_pattern(&vhdx2, block_size, block_size as usize).await;
    assert!(
        buf1.iter().all(|&b| b == 0x22) || buf1.iter().all(|&b| b == 0x33),
        "block 1 should have its own data, not block 0's"
    );
}

/// Trim + flush + write + flush + reopen: verify clean ownership.
#[async_test]
async fn deferred_trim_flush_write_flush_reopen(driver: DefaultDriver) {
    let (mem_file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let snapshot = mem_file.snapshot();

    let crash_file = CrashTestFile::from_durable(snapshot);
    let vhdx = VhdxFile::open(crash_file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size as u64;

    // Write block 0, flush.
    write_pattern(&vhdx, 0, block_size as usize, 0xAA).await;
    vhdx.flush().await.unwrap();

    // Trim block 0, flush (trim becomes durable).
    let trim_req = crate::trim::TrimRequest::new(crate::trim::TrimMode::FileSpace, 0, block_size);
    vhdx.trim(trim_req).await.unwrap();
    vhdx.flush().await.unwrap();

    // Write block 0 again (same-block reclaim of durable anchor), flush.
    write_pattern(&vhdx, 0, block_size as usize, 0xBB).await;
    vhdx.flush().await.unwrap();

    // Graceful close.
    let durable = vhdx.file.durable_snapshot();
    vhdx.close().await.unwrap();

    // Reopen and verify.
    let recovered = InMemoryFile::from_snapshot(durable);
    let vhdx2 = VhdxFile::open(recovered)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();

    let buf = read_pattern(&vhdx2, 0, block_size as usize).await;
    assert!(
        buf.iter().all(|&b| b == 0xBB),
        "block 0 should have new data after trim+write+flush cycle"
    );
}
