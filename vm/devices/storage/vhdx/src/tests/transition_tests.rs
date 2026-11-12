// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::support::InMemoryFile;
use super::support::TestIo;
use super::support::YieldingCrashFile;
use crate::AsyncFile;
use crate::AsyncFileExt;
use crate::CreateParams;
use crate::ReadRange;
use crate::VhdxFile;
use crate::WriteRange;
use crate::create;
use crate::format::BatEntryState;
use crate::format::MB1;
use crate::format::SECTORS_PER_CHUNK;
use crate::header::WriteMode;
use crate::trim::TrimMode;
use crate::trim::TrimRequest;
use futures::FutureExt;
use pal_async::DefaultDriver;
use pal_async::async_test;

async fn disk(
    driver: &DefaultDriver,
    has_parent: bool,
    sector_size: u32,
) -> VhdxFile<YieldingCrashFile> {
    disk_sized(driver, has_parent, sector_size, 4 * MB1).await
}

async fn disk_sized(
    driver: &DefaultDriver,
    has_parent: bool,
    sector_size: u32,
    disk_size: u64,
) -> VhdxFile<YieldingCrashFile> {
    let file = InMemoryFile::new(0);
    create(
        &file,
        &mut CreateParams {
            disk_size,
            block_size: MB1 as u32,
            logical_sector_size: sector_size,
            has_parent,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let disk = VhdxFile::open(YieldingCrashFile::from_durable(
        file.snapshot(),
        false,
        false,
    ))
    .writable(driver)
    .await
    .unwrap();
    disk.enable_write_mode(WriteMode::DataWritable)
        .await
        .unwrap();
    disk
}

async fn write_ranges(disk: &VhdxFile<YieldingCrashFile>, ranges: &[WriteRange], value: u8) {
    for range in ranges {
        let (offset, length, value) = match *range {
            WriteRange::Data {
                file_offset,
                length,
                ..
            } => (file_offset, length, value),
            WriteRange::Zero {
                file_offset,
                length,
            } => (file_offset, length, 0),
        };
        disk.file
            .write_at(offset, &vec![value; length as usize])
            .await
            .unwrap();
    }
}

async fn write(disk: &VhdxFile<YieldingCrashFile>, offset: u64, length: u32, value: u8) {
    let mut ranges = Vec::new();
    let guard = disk
        .resolve_write(offset, length, &mut ranges)
        .await
        .unwrap();
    write_ranges(disk, &ranges, value).await;
    guard.complete().await.unwrap();
}

fn parent_byte(sector: u64) -> u8 {
    0x80 | (sector % 64) as u8
}

async fn read<F: AsyncFile>(disk: &VhdxFile<F>, offset: u64, length: u32) -> Vec<u8> {
    let sector_size = disk.logical_sector_size() as usize;
    let mut actual = vec![0; length as usize];
    for (i, sector) in actual.chunks_exact_mut(sector_size).enumerate() {
        sector.fill(parent_byte(offset / sector_size as u64 + i as u64));
    }
    let mut ranges = Vec::new();
    let _guard = disk
        .resolve_read(offset, length, &mut ranges)
        .await
        .unwrap();
    for range in ranges {
        match range {
            ReadRange::Data {
                guest_offset,
                length,
                file_offset,
            } => {
                let start = (guest_offset - offset) as usize;
                disk.file
                    .read_at(file_offset, &mut actual[start..start + length as usize])
                    .await
                    .unwrap();
            }
            ReadRange::Zero {
                guest_offset,
                length,
            } => {
                let start = (guest_offset - offset) as usize;
                actual[start..start + length as usize].fill(0);
            }
            ReadRange::Unmapped { .. } => {}
        }
    }
    actual
}

#[async_test]
async fn recycled_bitmap_initialization_is_logged_without_whole_block_zeroing(
    driver: DefaultDriver,
) {
    for sector_size in [512, 4096] {
        for volatile_zero in [false, true] {
            let disk = disk(&driver, true, sector_size).await;
            for block in [0, 3] {
                write(&disk, block * MB1, disk.block_size(), 0xFF).await;
            }
            let bitmap_offset = disk.bat.get_block_mapping(3).file_offset();
            disk.trim(TrimRequest::new(TrimMode::FreeSpace, 3 * MB1, MB1))
                .await
                .unwrap();
            disk.flush().await.unwrap();
            if volatile_zero {
                // A zero read is not proof of durable initialization.
                disk.file.zero_range(bitmap_offset, MB1).await.unwrap();
            }
            disk.file.persist_except(bitmap_offset..bitmap_offset + MB1);
            // Reclaim safe same-block payload space so the only recycled
            // metadata initialization here is the SBM, not a foreign payload.
            disk.trim(TrimRequest::new(TrimMode::MakeTransparent, 0, MB1))
                .await
                .unwrap();
            let before = disk
                .log_state
                .as_ref()
                .unwrap()
                .flush_sequencer
                .completed_fsn();
            let mut ranges = Vec::new();
            let guard = disk
                .resolve_write(0, sector_size, &mut ranges)
                .await
                .unwrap();
            assert_eq!(
                disk.bat.get_sector_bitmap_mapping(0).file_offset(),
                bitmap_offset
            );
            let lsn = disk.cache.commit().unwrap();
            disk.log_state
                .as_ref()
                .unwrap()
                .logged_lsn
                .wait_for(lsn)
                .await
                .unwrap();
            assert_eq!(
                disk.log_state
                    .as_ref()
                    .unwrap()
                    .flush_sequencer
                    .completed_fsn(),
                before,
                "bitmap initialization should use WAL rather than a pre-log data flush"
            );
            let snapshot = disk.file.durable_snapshot();
            drop(guard);
            disk.abort().await;
            let recovered = VhdxFile::open(InMemoryFile::from_snapshot(snapshot))
                .allow_replay(true)
                .read_only()
                .await
                .unwrap();
            assert_eq!(
                read(&recovered, 0, sector_size).await,
                vec![parent_byte(0); sector_size as usize],
                "initialization was not recovered from WAL"
            );
            let mut unused = [0];
            recovered
                .file
                .read_at(bitmap_offset + 4096, &mut unused)
                .await
                .unwrap();
            assert_eq!(unused[0], 0xFF, "unused bitmap pages need not be zeroed");

            let next = VhdxFile::open(YieldingCrashFile::from_durable(
                recovered.file.snapshot(),
                false,
                false,
            ))
            .writable(&driver)
            .await
            .unwrap();
            write(&next, MB1 + sector_size as u64, sector_size, 0x61).await;
            assert_eq!(
                read(&next, MB1, sector_size).await,
                vec![parent_byte(MB1 / sector_size as u64); sector_size as usize],
                "allocating a neighbor revived its stale presence bits"
            );
            next.close().await.unwrap();
        }
    }
}

#[async_test]
async fn bitmap_initialization_preserves_neighbors_across_boundaries(driver: DefaultDriver) {
    for sector_size in [512, 4096] {
        let chunk_blocks = SECTORS_PER_CHUNK * sector_size as u64 / MB1;
        let page_blocks = 4096 * 8 * sector_size as u64 / MB1;
        let disk = disk_sized(&driver, true, sector_size, (chunk_blocks + 2) * MB1).await;
        let blocks = [
            0,
            1,
            page_blocks - 1,
            page_blocks,
            chunk_blocks - 1,
            chunk_blocks,
        ];
        for (i, block) in blocks.into_iter().enumerate() {
            write(&disk, block * MB1, sector_size, 0x31 + i as u8).await;
        }
        for block in [0, page_blocks, chunk_blocks] {
            disk.trim(TrimRequest::new(
                TrimMode::MakeTransparent,
                block * MB1,
                MB1,
            ))
            .await
            .unwrap();
            let a = write(&disk, block * MB1 + sector_size as u64, sector_size, 0x61);
            let b = write(
                &disk,
                block * MB1 + 2 * sector_size as u64,
                sector_size,
                0x62,
            );
            let ((), (), result) = futures::join!(a, b, disk.flush());
            result.unwrap();
        }
        disk.flush().await.unwrap();
        let snapshot = disk.file.durable_snapshot();
        disk.abort().await;
        let recovered = VhdxFile::open(InMemoryFile::from_snapshot(snapshot))
            .allow_replay(true)
            .read_only()
            .await
            .unwrap();
        for (i, block) in blocks.into_iter().enumerate() {
            let offset = block * MB1;
            let rewritten = [0, page_blocks, chunk_blocks].contains(&block);
            let expected = if rewritten {
                parent_byte(offset / sector_size as u64)
            } else {
                0x31 + i as u8
            };
            assert_eq!(
                read(&recovered, offset, sector_size).await,
                vec![expected; sector_size as usize]
            );
            if rewritten {
                for (sector, value) in [(1, 0x61), (2, 0x62)] {
                    assert_eq!(
                        read(
                            &recovered,
                            offset + sector * sector_size as u64,
                            sector_size
                        )
                        .await,
                        vec![value; sector_size as usize]
                    );
                }
            }
        }
    }
}

fn flush_count(file: &YieldingCrashFile) -> usize {
    file.trace()
        .iter()
        .filter(|(operation, _)| *operation == TestIo::Flush)
        .count()
}

#[async_test]
async fn safe_partial_payloads_do_not_require_recurring_data_flushes(driver: DefaultDriver) {
    for sector_size in [512, 4096] {
        let disk = disk(&driver, true, sector_size).await;
        disk.file.schedule(1, false);
        for sector in [0, 1, 0] {
            let before = flush_count(&disk.file);
            write(&disk, sector * sector_size as u64, sector_size, 0x61).await;
            disk.flush().await.unwrap();
            assert_eq!(
                flush_count(&disk.file) - before,
                1,
                "safe payload needs only the final durability flush"
            );
        }
        write(&disk, 2 * MB1, disk.block_size(), 0xE7).await;
        disk.trim(TrimRequest::new(TrimMode::FreeSpace, 2 * MB1, MB1))
            .await
            .unwrap();
        disk.flush().await.unwrap();
        let before = flush_count(&disk.file);
        write(&disk, MB1, sector_size, 0x62).await;
        disk.flush().await.unwrap();
        assert_eq!(
            flush_count(&disk.file) - before,
            2,
            "foreign recycled space has one initialization barrier"
        );
        let before = flush_count(&disk.file);
        write(&disk, MB1 + sector_size as u64, sector_size, 0x63).await;
        disk.flush().await.unwrap();
        assert_eq!(
            flush_count(&disk.file) - before,
            1,
            "the allocation barrier must not recur for later sectors"
        );
        let snapshot = disk.file.durable_snapshot();
        disk.abort().await;
        let reopened = VhdxFile::open(YieldingCrashFile::from_durable(snapshot, false, false))
            .writable(&driver)
            .await
            .unwrap();
        reopened
            .enable_write_mode(WriteMode::DataWritable)
            .await
            .unwrap();
        reopened.file.schedule(1, false);
        let before = flush_count(&reopened.file);
        write(&reopened, MB1 + 2 * sector_size as u64, sector_size, 0x64).await;
        reopened.flush().await.unwrap();
        assert_eq!(
            flush_count(&reopened.file) - before,
            1,
            "the safe allocation invariant survives reopen"
        );
        assert!(
            read(&reopened, MB1, sector_size)
                .await
                .iter()
                .all(|&b| b == 0x62)
        );
        reopened.close().await.unwrap();
    }
}

#[async_test]
async fn partial_allocation_initializes_payload_before_bitmap_publication(driver: DefaultDriver) {
    for sector_size in [512, 4096] {
        for intervening_flush in [false, true] {
            let disk = disk(&driver, true, sector_size).await;
            write(&disk, 2 * MB1, disk.block_size(), 0xE7).await;
            let stale_offset = disk.bat.get_block_mapping(2).file_offset();
            disk.trim(TrimRequest::new(TrimMode::FreeSpace, 2 * MB1, MB1))
                .await
                .unwrap();
            disk.flush().await.unwrap();
            disk.file.persist_except(stale_offset..stale_offset + MB1);

            let mut ranges = Vec::new();
            let guard = disk
                .resolve_write(0, sector_size, &mut ranges)
                .await
                .unwrap();
            assert_eq!(disk.bat.get_block_mapping(0).file_offset(), stale_offset);
            let mut initialized = vec![0; disk.block_size() as usize];
            disk.file
                .read_at(stale_offset, &mut initialized)
                .await
                .unwrap();
            assert!(
                initialized.iter().all(|&b| b == 0),
                "recycled partial payload was not initialized"
            );
            if intervening_flush {
                disk.flush().await.unwrap();
            } else {
                // Merely logging the BAT must make initialization durable;
                // no explicit caller flush is needed to enforce the barrier.
                let lsn = disk.cache.commit().unwrap();
                disk.log_state
                    .as_ref()
                    .unwrap()
                    .logged_lsn
                    .wait_for(lsn)
                    .await
                    .unwrap();
            }
            write_ranges(&disk, &ranges, 0x61).await;
            guard.complete().await.unwrap();
            let lsn = disk.cache.commit().unwrap();
            disk.log_state
                .as_ref()
                .unwrap()
                .logged_lsn
                .wait_for(lsn)
                .await
                .unwrap();
            let snapshot = disk.file.durable_snapshot();
            disk.abort().await;
            let recovered = VhdxFile::open(InMemoryFile::from_snapshot(snapshot))
                .allow_replay(true)
                .read_only()
                .await
                .unwrap();
            let actual = read(&recovered, 0, sector_size).await;
            assert!(
                actual.iter().all(|&b| b == 0),
                "lost unflushed data may reveal only the initialized zero baseline"
            );
        }
    }
}

#[async_test]
async fn cancelled_or_failed_payload_initialization_can_be_retried(driver: DefaultDriver) {
    for cancel in [true, false] {
        let disk = disk(&driver, true, 512).await;
        write(&disk, 2 * MB1, disk.block_size(), 0xE7).await;
        let stale_offset = disk.bat.get_block_mapping(2).file_offset();
        disk.trim(TrimRequest::new(TrimMode::FreeSpace, 2 * MB1, MB1))
            .await
            .unwrap();
        disk.flush().await.unwrap();
        let mut ranges = Vec::new();
        if cancel {
            disk.file.pause(TestIo::Write, stale_offset);
            let mut resolve = Box::pin(disk.resolve_write(0, 512, &mut ranges));
            assert!(futures::poll!(&mut resolve).is_pending());
            drop(resolve);
            disk.file.resume();
        } else {
            disk.file.fail_next(TestIo::Write, stale_offset);
            assert!(disk.resolve_write(0, 512, &mut ranges).await.is_err());
        }
        assert_eq!(
            disk.bat.get_block_mapping(0).bat_state(),
            BatEntryState::NotPresent
        );
        assert_eq!(disk.bat.io_refcount(0), 0);
        assert_eq!(read(&disk, 0, 512).await, vec![parent_byte(0); 512]);
        write(&disk, 0, 512, 0x61).await;
        assert_eq!(disk.bat.get_block_mapping(0).file_offset(), stale_offset);
        disk.flush().await.unwrap();
        assert!(read(&disk, 0, 512).await.iter().all(|&b| b == 0x61));
        disk.close().await.unwrap();
    }
}

#[async_test]
async fn cancelled_resolve_reverts_preceding_allocations(driver: DefaultDriver) {
    let disk = disk(&driver, false, 512).await;
    let target = {
        let mut eof = disk.allocation_lock.lock().await;
        eof.eof_extension_length = disk.block_size();
        assert_eq!(eof.zero_offset, eof.file_length);
        eof.file_length + 2 * disk.block_size() as u64
    };
    disk.file.pause(TestIo::Resize, target);
    let mut ranges = Vec::new();
    let mut resolve = Box::pin(disk.resolve_write(0, 2 * disk.block_size(), &mut ranges));
    assert!(futures::poll!(&mut resolve).is_pending());
    assert!(
        disk.bat
            .get_block_mapping(0)
            .transitioning_to_fully_present()
    );
    drop(resolve);
    assert!(
        !disk
            .bat
            .get_block_mapping(0)
            .transitioning_to_fully_present(),
        "cancelling resolve stranded an earlier allocation"
    );
    assert_eq!(disk.bat.io_refcount(0), 0);
    disk.file.resume();
    write(&disk, 0, 2 * disk.block_size(), 0x51).await;
    disk.close().await.unwrap();
}

#[async_test]
async fn cancelled_complete_reverts_allocation_and_cache_permit_wait(driver: DefaultDriver) {
    let disk = disk(&driver, false, 512).await;
    let mut ranges = Vec::new();
    let guard = disk
        .resolve_write(0, disk.block_size(), &mut ranges)
        .await
        .unwrap();
    write_ranges(&disk, &ranges, 0x51).await;
    let permits = &disk.log_state.as_ref().unwrap().log_permits;
    let held = permits.available();
    permits.acquire(held).await.unwrap();

    let mut complete = Box::pin(guard.complete());
    assert!(futures::poll!(&mut complete).is_pending());
    drop(complete);
    permits.release(held);
    assert!(
        !disk
            .bat
            .get_block_mapping(0)
            .transitioning_to_fully_present(),
        "cancelling complete stranded TFP"
    );
    let mut ranges = Vec::new();
    let guard = disk
        .resolve_write(0, disk.block_size(), &mut ranges)
        .await
        .unwrap();
    let mut retry = Box::pin(guard.complete());
    assert!(
        matches!(futures::poll!(&mut retry), std::task::Poll::Ready(Ok(()))),
        "cancelled cache acquisition stranded its permit state"
    );
    drop(retry);
    disk.close().await.unwrap();
}

#[async_test]
async fn cancelled_or_failed_bitmap_load_can_be_retried(driver: DefaultDriver) {
    for cancel in [true, false] {
        for sector_size in [512, 4096] {
            let disk = disk(&driver, true, sector_size).await;
            write(&disk, 0, sector_size, 0x61).await;
            disk.flush().await.unwrap();
            let snapshot = disk.file.durable_snapshot();
            disk.abort().await;
            let disk = VhdxFile::open(YieldingCrashFile::from_durable(snapshot, false, false))
                .writable(&driver)
                .await
                .unwrap();
            let bitmap_offset = disk.bat.get_sector_bitmap_mapping(0).file_offset();
            let mut ranges = Vec::new();
            if cancel {
                disk.file.pause(TestIo::Read, bitmap_offset);
                let mut resolve = Box::pin(disk.resolve_read(0, sector_size, &mut ranges));
                assert!(futures::poll!(&mut resolve).is_pending());
                drop(resolve);
                disk.file.resume();
            } else {
                disk.file.fail_next(TestIo::Read, bitmap_offset);
                assert!(
                    disk.resolve_read(0, sector_size, &mut ranges)
                        .await
                        .is_err()
                );
            }

            let mut ranges = Vec::new();
            let mut retry = Box::pin(disk.resolve_read(0, sector_size, &mut ranges));
            assert!(
                matches!(futures::poll!(&mut retry), std::task::Poll::Ready(Ok(_))),
                "bitmap load cancellation/failure left the cache permanently loading"
            );
            drop(retry);
            assert!(read(&disk, 0, sector_size).await.iter().all(|&b| b == 0x61));
            disk.close().await.unwrap();
        }
    }
}

#[async_test]
async fn cancelled_completion_preserves_already_published_blocks(driver: DefaultDriver) {
    let disk = disk_sized(&driver, false, 512, 514 * MB1).await;
    let mut ranges = Vec::new();
    let guard = disk
        .resolve_write(511 * MB1, 2 * MB1 as u32, &mut ranges)
        .await
        .unwrap();
    write_ranges(&disk, &ranges, 0x51).await;
    let permits = &disk.log_state.as_ref().unwrap().log_permits;
    let held = permits.available() - 1;
    permits.acquire(held).await.unwrap();
    let mut complete = Box::pin(guard.complete());
    assert!(futures::poll!(&mut complete).is_pending());
    assert!(
        !disk
            .bat
            .get_block_mapping(512)
            .transitioning_to_fully_present()
    );
    assert!(
        disk.bat
            .get_block_mapping(511)
            .transitioning_to_fully_present()
    );
    drop(complete);
    permits.release(held);
    assert!(
        !disk
            .bat
            .get_block_mapping(511)
            .transitioning_to_fully_present()
    );
    write(&disk, 511 * MB1, disk.block_size(), 0x62).await;
    assert!(read(&disk, 512 * MB1, 512).await.iter().all(|&b| b == 0x51));
    disk.close().await.unwrap();
}

#[async_test]
async fn cancelled_partial_resolve_restores_unpublished_payload(driver: DefaultDriver) {
    let disk = disk(&driver, true, 512).await;
    write(&disk, MB1, 512, 0x51).await;
    disk.flush().await.unwrap();
    let snapshot = disk.file.durable_snapshot();
    disk.abort().await;
    let disk = VhdxFile::open(YieldingCrashFile::from_durable(snapshot, false, false))
        .writable(&driver)
        .await
        .unwrap();
    let expected_offset = disk.allocation_lock.lock().await.zero_offset;
    disk.file.pause(
        TestIo::Read,
        disk.bat.get_sector_bitmap_mapping(0).file_offset(),
    );
    let mut ranges = Vec::new();
    let mut resolve = Box::pin(disk.resolve_write(0, 512, &mut ranges));
    assert!(futures::poll!(&mut resolve).is_pending());
    drop(resolve);
    disk.file.resume();
    {
        let eof = disk.allocation_lock.lock().await;
        assert!(
            !disk
                .free_space
                .is_range_in_use(&eof, expected_offset, disk.block_size())
        );
    }
    let mut ranges = Vec::new();
    let guard = disk.resolve_write(0, 512, &mut ranges).await.unwrap();
    assert_eq!(disk.bat.get_block_mapping(0).file_offset(), expected_offset);
    write_ranges(&disk, &ranges, 0x62).await;
    guard.complete().await.unwrap();
    assert!(read(&disk, MB1, 512).await.iter().all(|&b| b == 0x51));
    disk.close().await.unwrap();
}

#[async_test]
async fn cancelled_read_load_is_pinned_until_cleanup() {
    use crate::cache::PageCache;
    use crate::cache::PageKey;
    use std::sync::Arc;

    let file = Arc::new(YieldingCrashFile::from_durable(
        vec![0x51; 8192],
        false,
        false,
    ));
    let mut cache = PageCache::new(file.clone(), None, None, 1);
    cache.register_tag(0, 0);
    file.pause(TestIo::Read, 0);
    let mut pending = Box::pin(cache.acquire_read(PageKey { tag: 0, offset: 0 }));
    assert!(futures::poll!(&mut pending).is_pending());
    drop(
        cache
            .acquire_read(PageKey {
                tag: 0,
                offset: 4096,
            })
            .await
            .unwrap(),
    );
    drop(pending);
    file.resume();
    let mut retry = Box::pin(cache.acquire_read(PageKey { tag: 0, offset: 0 }));
    assert!(
        matches!(futures::poll!(&mut retry), std::task::Poll::Ready(Ok(_))),
        "cancelled read load could not be retried after cache pressure"
    );
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModelBlock {
    Transparent,
    Zero,
    Present,
}

struct SectorModel {
    live: Vec<u8>,
    allowed: Vec<[u64; 4]>,
    blocks: [ModelBlock; 4],
}

impl SectorModel {
    fn new(sectors: usize) -> Self {
        let live = (0..sectors).map(|i| parent_byte(i as u64)).collect();
        let mut model = Self {
            live,
            allowed: vec![[0; 4]; sectors],
            blocks: [ModelBlock::Transparent; 4],
        };
        model.flush();
        model
    }

    fn update(&mut self, start: usize, count: usize, value: impl Fn(usize) -> u8) {
        for sector in start..start + count {
            let value = value(sector);
            self.live[sector] = value;
            self.allowed[sector][value as usize / 64] |= 1 << (value % 64);
        }
    }

    fn flush(&mut self) {
        for (allowed, &value) in self.allowed.iter_mut().zip(&self.live) {
            *allowed = [0; 4];
            allowed[value as usize / 64] = 1 << (value % 64);
        }
    }

    fn check(&self, bytes: &[u8], sector_size: usize, crash: bool, context: &str) {
        for (sector, data) in bytes.chunks_exact(sector_size).enumerate() {
            let value = data[0];
            let valid = if crash {
                self.allowed[sector][value as usize / 64] & (1 << (value % 64)) != 0
            } else {
                value == self.live[sector]
            };
            assert!(
                valid && data.iter().all(|&b| b == value),
                "{context}: sector {sector} value {value:#x}, live {:#x}, allowed {:?}",
                self.live[sector],
                self.allowed[sector]
            );
        }
    }
}

fn random(state: &mut u64) -> u64 {
    *state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
    *state >> 32
}

async fn check_ownership(disk: &VhdxFile<YieldingCrashFile>, context: &str) {
    let eof = disk.allocation_lock.lock().await;
    let mut ranges = Vec::new();
    for block in 0..4 {
        let mapping = disk.bat.get_block_mapping(block);
        assert!(
            !mapping.transitioning_to_fully_present(),
            "{context}: stranded TFP on {block}"
        );
        assert_eq!(
            disk.bat.io_refcount(block),
            0,
            "{context}: leaked I/O reference"
        );
        if mapping.file_megabyte() != 0 {
            let offset = mapping.file_offset();
            assert!(
                disk.free_space
                    .is_range_in_use(&eof, offset, disk.block_size()),
                "{context}: referenced space is free"
            );
            ranges.push((offset, offset + disk.block_size() as u64));
        }
    }
    let bitmap = disk.bat.get_sector_bitmap_mapping(0);
    if bitmap.file_megabyte() != 0 {
        ranges.push((bitmap.file_offset(), bitmap.file_offset() + MB1));
    }
    ranges.sort_unstable();
    assert!(
        ranges.windows(2).all(|pair| pair[0].1 <= pair[1].0),
        "{context}: overlapping allocations {ranges:?}"
    );
}

async fn model_run(driver: &DefaultDriver, seed: u64, sector_size: u32, persist: bool) {
    let mut disk = disk(driver, true, sector_size).await;
    disk.file.schedule(seed, persist);
    let sectors_per_block = disk.block_size() as usize / sector_size as usize;
    let mut model = SectorModel::new(4 * sectors_per_block);
    let mut rng = seed;
    let mut history = Vec::new();
    for step in 0..40 {
        let block = random(&mut rng) as usize % 4;
        // Distinct blocks use disjoint value ranges, so foreign payload
        // cannot masquerade as an allowed unflushed write to this block.
        let value = 1 + block as u8 * 16 + step as u8 % 15;
        let sector = random(&mut rng) as usize % (sectors_per_block - 1);
        let operation = if step % 8 == 7 {
            7
        } else {
            random(&mut rng) % 7
        };
        history.push((operation, block, sector, value));
        let offset = block as u64 * MB1;
        let base = block * sectors_per_block;
        match operation {
            0 | 1 => {
                let full = operation == 1;
                let start = if full { base } else { base + sector };
                let count = if full { sectors_per_block } else { 1 };
                write(
                    &disk,
                    start as u64 * sector_size as u64,
                    (count * sector_size as usize) as u32,
                    value,
                )
                .await;
                // Fresh FullyPresent publication may expose initial zeros
                // if the new payload write has not persisted.
                if full {
                    for allowed in &mut model.allowed[start..start + count] {
                        allowed[0] |= 1;
                    }
                } else if model.live[start] == parent_byte(start as u64) {
                    // A persisted presence bit can expose the initialized
                    // zero baseline if this unflushed data write is lost.
                    model.allowed[start][0] |= 1;
                }
                model.update(start, count, |_| value);
                model.blocks[block] = ModelBlock::Present;
            }
            2 => {
                let a = write(
                    &disk,
                    offset + sector as u64 * sector_size as u64,
                    sector_size,
                    value,
                );
                let b = write(
                    &disk,
                    offset + (sector + 1) as u64 * sector_size as u64,
                    sector_size,
                    value + 1,
                );
                futures::join!(a, b);
                for i in [base + sector, base + sector + 1] {
                    if model.live[i] == parent_byte(i as u64) {
                        model.allowed[i][0] |= 1;
                    }
                }
                model.update(base + sector, 1, |_| value);
                model.update(base + sector + 1, 1, |_| value + 1);
                model.blocks[block] = ModelBlock::Present;
            }
            3 => {
                let modes = [
                    TrimMode::MakeTransparent,
                    TrimMode::Zero,
                    TrimMode::FreeSpace,
                    TrimMode::FileSpace,
                ];
                let mode = modes[random(&mut rng) as usize % modes.len()];
                disk.trim(TrimRequest::new(mode, offset, MB1))
                    .await
                    .unwrap();
                if mode == TrimMode::MakeTransparent {
                    model.update(base, sectors_per_block, |i| parent_byte(i as u64));
                    model.blocks[block] = ModelBlock::Transparent;
                } else if mode == TrimMode::Zero || model.blocks[block] != ModelBlock::Transparent {
                    model.update(base, sectors_per_block, |_| 0);
                    model.blocks[block] = ModelBlock::Zero;
                }
            }
            4 => {
                let mut ranges = Vec::new();
                let length = if sector.is_multiple_of(2) {
                    disk.block_size()
                } else {
                    sector_size
                };
                let guard = disk
                    .resolve_write(offset, length, &mut ranges)
                    .await
                    .unwrap();
                drop(guard);
                if length == sector_size && model.blocks[block] == ModelBlock::Transparent {
                    model.blocks[block] = ModelBlock::Present;
                }
            }
            5 => {
                // All writes finish before this barrier; their values are
                // mandatory after recovery, not just allowed candidates.
                disk.flush().await.unwrap();
                model.flush();
            }
            6 => {
                let actual = read(&disk, 0, 4 * MB1 as u32).await;
                model.check(
                    &actual,
                    sector_size as usize,
                    false,
                    &format!("seed {seed}, step {step}, history {history:?}"),
                );
            }
            7 => {
                let snapshot = disk.file.durable_snapshot();
                let trace = disk.file.trace();
                disk.abort().await;
                let recovered = VhdxFile::open(InMemoryFile::from_snapshot(snapshot))
                    .allow_replay(true)
                    .read_only()
                    .await
                    .unwrap();
                let actual = read(&recovered, 0, 4 * MB1 as u32).await;
                model.check(
                    &actual,
                    sector_size as usize,
                    true,
                    &format!("seed {seed}, step {step}, history {history:?}, I/O {trace:?}"),
                );
                for (value, data) in model
                    .live
                    .iter_mut()
                    .zip(actual.chunks_exact(sector_size as usize))
                {
                    *value = data[0];
                }
                model.flush();
                let snapshot = recovered.file.snapshot();
                for block in 0..4 {
                    model.blocks[block] =
                        match recovered.bat.get_block_mapping(block as u32).bat_state() {
                            BatEntryState::NotPresent => ModelBlock::Transparent,
                            BatEntryState::FullyPresent | BatEntryState::PartiallyPresent => {
                                ModelBlock::Present
                            }
                            _ => ModelBlock::Zero,
                        };
                }
                drop(recovered);
                disk = VhdxFile::open(YieldingCrashFile::from_durable(snapshot, false, false))
                    .writable(driver)
                    .await
                    .unwrap();
                disk.file.schedule(seed ^ step, persist);
            }
            _ => unreachable!(),
        }
        check_ownership(
            &disk,
            &format!("seed {seed}, step {step}, history {history:?}"),
        )
        .await;
    }
    let actual = read(&disk, 0, 4 * MB1 as u32).await;
    model.check(
        &actual,
        sector_size as usize,
        false,
        &format!("seed {seed}, final history {history:?}"),
    );
    disk.close().await.unwrap();
}

#[async_test]
async fn seeded_differencing_transition_model(driver: DefaultDriver) {
    for seed in [1, 7, 0xBAD5EED, 0xDEADBEEF] {
        for sector_size in [512, 4096] {
            for persist in [false, true] {
                let result =
                    std::panic::AssertUnwindSafe(model_run(&driver, seed, sector_size, persist))
                        .catch_unwind()
                        .await;
                assert!(
                    result.is_ok(),
                    "model failed: seed {seed}, sector size {sector_size}, early persistence {persist}"
                );
            }
        }
    }
}
