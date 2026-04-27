// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::AsyncFile;
use crate::AsyncFileExt;
use crate::create::{self, CreateParams};
use crate::error::VhdxIoError;
use crate::error::VhdxIoErrorInner;
use crate::format;
use crate::format::BatEntry;
use crate::format::BatEntryState;
use crate::format::MB1;
use crate::header::WriteMode;
use crate::io::ReadRange;
use crate::io::WriteRange;
use crate::open::VhdxFile;
use crate::region;
use crate::tests::support::InMemoryFile;
use crate::tests::support::IoInterceptor;
use guid::Guid;
use pal_async::DefaultDriver;
use pal_async::async_test;
use std::borrow::Borrow;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use zerocopy::IntoBytes;

#[async_test]
async fn read_empty_disk_returns_zero() {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).read_only().await.unwrap();

    let mut ranges = Vec::new();
    let _guard = vhdx.resolve_read(0, 4096, &mut ranges).await.unwrap();

    assert_eq!(ranges.len(), 1);
    assert_eq!(
        ranges[0],
        ReadRange::Zero {
            guest_offset: 0,
            length: 4096,
        }
    );
}

#[async_test]
async fn read_zero_length() {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).read_only().await.unwrap();

    let mut ranges = Vec::new();
    let _guard = vhdx.resolve_read(0, 0, &mut ranges).await.unwrap();

    assert!(ranges.is_empty());
}

#[async_test]
async fn read_beyond_end_of_disk() {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).read_only().await.unwrap();

    let mut ranges = Vec::new();
    // Read 512 bytes past the end (both offset and length are sector-aligned).
    let result = vhdx
        .resolve_read(format::GB1 - 512, 1024, &mut ranges)
        .await;
    assert!(matches!(
        result,
        Err(VhdxIoError(VhdxIoErrorInner::BeyondEndOfDisk))
    ));
}

#[async_test]
async fn read_at_disk_end_exact() {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).read_only().await.unwrap();

    let mut ranges = Vec::new();
    let _guard = vhdx
        .resolve_read(format::GB1 - 4096, 4096, &mut ranges)
        .await
        .unwrap();

    assert_eq!(ranges.len(), 1);
    assert_eq!(
        ranges[0],
        ReadRange::Zero {
            guest_offset: format::GB1 - 4096,
            length: 4096,
        }
    );
}

#[async_test]
async fn read_fully_present_block() {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let regions = region::parse_region_tables(&file).await.unwrap();
    let bat_offset = regions.bat_offset;

    // Write a FullyPresent BAT entry for block 0 at file_offset_mb = 4.
    let entry = BatEntry::new()
        .with_state(BatEntryState::FullyPresent as u8)
        .with_file_offset_mb(4);
    file.write_at(bat_offset, entry.as_bytes()).await.unwrap();

    // Extend file to cover the allocated range.
    let needed = 4 * MB1 + format::DEFAULT_BLOCK_SIZE as u64;
    file.set_file_size(needed).await.unwrap();

    let vhdx = VhdxFile::open(file).read_only().await.unwrap();
    let mut ranges = Vec::new();
    let _guard = vhdx.resolve_read(0, 4096, &mut ranges).await.unwrap();

    assert_eq!(ranges.len(), 1);
    assert_eq!(
        ranges[0],
        ReadRange::Data {
            guest_offset: 0,
            length: 4096,
            file_offset: 4 * MB1,
        }
    );
}

#[async_test]
async fn read_spanning_two_blocks() {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).read_only().await.unwrap();

    let block_size = vhdx.block_size() as u64;
    let mut ranges = Vec::new();
    // Read last 512 bytes of block 0 and first 512 bytes of block 1.
    let _guard = vhdx
        .resolve_read((block_size - 512) as u64, 1024, &mut ranges)
        .await
        .unwrap();

    assert_eq!(ranges.len(), 2);
    assert_eq!(
        ranges[0],
        ReadRange::Zero {
            guest_offset: block_size - 512,
            length: 512,
        }
    );
    assert_eq!(
        ranges[1],
        ReadRange::Zero {
            guest_offset: block_size,
            length: 512,
        }
    );
}

#[async_test]
async fn read_spanning_multiple_blocks() {
    // Use a small disk with 1 MiB blocks so spans are easier to test.
    let file = InMemoryFile::new(0);
    let block_size = MB1 as u32;
    let mut params = CreateParams {
        disk_size: 4 * MB1,
        block_size,
        ..Default::default()
    };
    create::create(&file, &mut params).await.unwrap();
    let vhdx = VhdxFile::open(file).read_only().await.unwrap();

    let mut ranges = Vec::new();
    // Read across blocks 0, 1, 2: start at 512 KiB, length = 2 MiB.
    // Block 0: 512 KiB remaining. Block 1: full 1 MiB. Block 2: 512 KiB.
    let start = MB1 / 2; // middle of block 0
    let len = (2 * MB1) as u32; // spans 3 blocks
    let _guard = vhdx.resolve_read(start, len, &mut ranges).await.unwrap();

    assert_eq!(ranges.len(), 3);
    // Block 0: remaining half
    assert_eq!(
        ranges[0],
        ReadRange::Zero {
            guest_offset: start,
            length: (MB1 / 2) as u32,
        }
    );
    // Block 1: full block
    assert_eq!(
        ranges[1],
        ReadRange::Zero {
            guest_offset: MB1,
            length: block_size,
        }
    );
    // Block 2: first half
    assert_eq!(
        ranges[2],
        ReadRange::Zero {
            guest_offset: 2 * MB1,
            length: (MB1 / 2) as u32,
        }
    );
}

#[async_test]
async fn read_unaligned_within_block() {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let regions = region::parse_region_tables(&file).await.unwrap();

    // Set block 0 to FullyPresent at file_offset_mb = 4.
    let entry = BatEntry::new()
        .with_state(BatEntryState::FullyPresent as u8)
        .with_file_offset_mb(4);
    file.write_at(regions.bat_offset, entry.as_bytes())
        .await
        .unwrap();

    // Extend file to cover the allocated range.
    let needed = 4 * MB1 + format::DEFAULT_BLOCK_SIZE as u64;
    file.set_file_size(needed).await.unwrap();

    let vhdx = VhdxFile::open(file).read_only().await.unwrap();
    let mut ranges = Vec::new();
    // Read 512 bytes starting at sector 10 (offset 5120).
    let _guard = vhdx.resolve_read(5120, 512, &mut ranges).await.unwrap();

    assert_eq!(ranges.len(), 1);
    assert_eq!(
        ranges[0],
        ReadRange::Data {
            guest_offset: 5120,
            length: 512,
            file_offset: 4 * MB1 + 5120,
        }
    );
}

#[async_test]
async fn read_differencing_not_present() {
    let file = InMemoryFile::new(0);
    let mut params = CreateParams {
        disk_size: format::GB1,
        has_parent: true,
        ..Default::default()
    };
    create::create(&file, &mut params).await.unwrap();
    let vhdx = VhdxFile::open(file).read_only().await.unwrap();

    let mut ranges = Vec::new();
    let _guard = vhdx.resolve_read(0, 4096, &mut ranges).await.unwrap();

    assert_eq!(ranges.len(), 1);
    assert_eq!(
        ranges[0],
        ReadRange::Unmapped {
            guest_offset: 0,
            length: 4096,
        }
    );
}

#[async_test]
async fn read_zero_state_block() {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let regions = region::parse_region_tables(&file).await.unwrap();

    // Set block 0 to Zero state.
    let entry = BatEntry::new()
        .with_state(BatEntryState::Zero as u8)
        .with_file_offset_mb(0);
    file.write_at(regions.bat_offset, entry.as_bytes())
        .await
        .unwrap();

    let vhdx = VhdxFile::open(file).read_only().await.unwrap();
    let mut ranges = Vec::new();
    let _guard = vhdx.resolve_read(0, 4096, &mut ranges).await.unwrap();

    assert_eq!(ranges.len(), 1);
    assert_eq!(
        ranges[0],
        ReadRange::Zero {
            guest_offset: 0,
            length: 4096,
        }
    );
}

#[async_test]
async fn read_unmapped_block() {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let regions = region::parse_region_tables(&file).await.unwrap();

    // Set block 0 to Unmapped (trimmed) state.
    let entry = BatEntry::new()
        .with_state(BatEntryState::Unmapped as u8)
        .with_file_offset_mb(0);
    file.write_at(regions.bat_offset, entry.as_bytes())
        .await
        .unwrap();

    let vhdx = VhdxFile::open(file).read_only().await.unwrap();
    let mut ranges = Vec::new();
    let _guard = vhdx.resolve_read(0, 4096, &mut ranges).await.unwrap();

    assert_eq!(ranges.len(), 1);
    assert_eq!(
        ranges[0],
        ReadRange::Zero {
            guest_offset: 0,
            length: 4096,
        }
    );
}

#[async_test]
async fn read_undefined_state_block() {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let regions = region::parse_region_tables(&file).await.unwrap();

    // Set block 0 to Undefined state (value 1).
    let entry = BatEntry::new()
        .with_state(BatEntryState::Undefined as u8)
        .with_file_offset_mb(0);
    file.write_at(regions.bat_offset, entry.as_bytes())
        .await
        .unwrap();

    let vhdx = VhdxFile::open(file).read_only().await.unwrap();
    let mut ranges = Vec::new();
    let _guard = vhdx.resolve_read(0, 4096, &mut ranges).await.unwrap();

    assert_eq!(ranges.len(), 1);
    assert_eq!(
        ranges[0],
        ReadRange::Zero {
            guest_offset: 0,
            length: 4096,
        }
    );
}

#[async_test]
async fn read_entire_disk() {
    // Small disk: 4 MiB with 2 MiB blocks = 2 blocks.
    let disk_size = 4 * MB1;
    let (file, _) = InMemoryFile::create_test_vhdx(disk_size).await;
    let vhdx = VhdxFile::open(file).read_only().await.unwrap();

    let mut ranges = Vec::new();
    let _guard = vhdx
        .resolve_read(0, disk_size as u32, &mut ranges)
        .await
        .unwrap();

    // 2 blocks, each produces one Zero range.
    assert_eq!(ranges.len(), 2);
    assert_eq!(
        ranges[0],
        ReadRange::Zero {
            guest_offset: 0,
            length: format::DEFAULT_BLOCK_SIZE,
        }
    );
    assert_eq!(
        ranges[1],
        ReadRange::Zero {
            guest_offset: format::DEFAULT_BLOCK_SIZE as u64,
            length: format::DEFAULT_BLOCK_SIZE,
        }
    );
}

#[async_test]
async fn read_4k_sector_disk() {
    let file = InMemoryFile::new(0);
    let mut params = CreateParams {
        disk_size: format::GB1,
        logical_sector_size: 4096,
        physical_sector_size: 4096,
        ..Default::default()
    };
    create::create(&file, &mut params).await.unwrap();
    let vhdx = VhdxFile::open(file).read_only().await.unwrap();

    let mut ranges = Vec::new();
    // Read one 4K sector.
    let _guard = vhdx.resolve_read(0, 4096, &mut ranges).await.unwrap();
    assert_eq!(ranges.len(), 1);
    assert_eq!(
        ranges[0],
        ReadRange::Zero {
            guest_offset: 0,
            length: 4096,
        }
    );

    // Unaligned read should fail.
    let mut ranges2 = Vec::new();
    let result = vhdx.resolve_read(512, 4096, &mut ranges2).await;
    assert!(matches!(
        result,
        Err(VhdxIoError(VhdxIoErrorInner::UnalignedIo))
    ));
}

// ---- Write tests ----

#[async_test]
async fn write_to_empty_block(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    let mut ranges = Vec::new();
    let _guard = vhdx.resolve_write(0, 4096, &mut ranges).await.unwrap();

    // Should allocate a new block. With SpaceState::Zero (near-EOF
    // extension space), zero padding is skipped — only Data emitted.
    // Writing 4096 bytes at offset 0 in block:
    //   Data(0, 4096, file_offset)
    assert!(!ranges.is_empty());
    // First should be Data
    match ranges[0] {
        WriteRange::Data {
            guest_offset,
            length,
            file_offset,
        } => {
            assert_eq!(guest_offset, 0);
            assert_eq!(length, 4096);
            // file_offset should be MB-aligned.
            assert!(file_offset > 0);
            assert_eq!(file_offset % MB1, 0);
        }
        _ => panic!("expected Data range, got {:?}", ranges[0]),
    }
    // With safe data, trailing zero padding is skipped.
    // If not safe, a trailing Zero range would follow.
    if ranges.len() > 1 {
        match ranges[1] {
            WriteRange::Zero {
                file_offset,
                length,
            } => {
                assert_eq!(length, format::DEFAULT_BLOCK_SIZE - 4096);
                assert!(file_offset > 0);
            }
            _ => panic!("expected Zero range, got {:?}", ranges[1]),
        }
    }
}

#[async_test]
async fn write_to_fully_present_block(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let regions = region::parse_region_tables(&file).await.unwrap();

    // Write a FullyPresent BAT entry for block 0 at file_offset_mb = 4.
    let entry = BatEntry::new()
        .with_state(BatEntryState::FullyPresent as u8)
        .with_file_offset_mb(4);
    file.write_at(regions.bat_offset, entry.as_bytes())
        .await
        .unwrap();

    // Extend file to cover the allocated range.
    let needed = 4 * MB1 + format::DEFAULT_BLOCK_SIZE as u64;
    file.set_file_size(needed).await.unwrap();

    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let mut ranges = Vec::new();
    let _guard = vhdx.resolve_write(0, 4096, &mut ranges).await.unwrap();

    // Should write directly to the existing block — single Data range.
    assert_eq!(ranges.len(), 1);
    assert_eq!(
        ranges[0],
        WriteRange::Data {
            guest_offset: 0,
            length: 4096,
            file_offset: 4 * MB1,
        }
    );
}

#[async_test]
async fn write_spanning_two_blocks(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    let block_size = vhdx.block_size() as u64;
    let mut ranges = Vec::new();
    // Write last 512 bytes of block 0 and first 512 bytes of block 1.
    let _guard = vhdx
        .resolve_write((block_size - 512) as u64, 1024, &mut ranges)
        .await
        .unwrap();

    // Each block needs allocation. Filter out the data ranges.
    let data_ranges: Vec<_> = ranges
        .iter()
        .filter(|r| matches!(r, WriteRange::Data { .. }))
        .collect();
    assert_eq!(data_ranges.len(), 2, "expected 2 Data ranges for 2 blocks");

    // First Data: last 512 bytes of block 0.
    match data_ranges[0] {
        WriteRange::Data {
            guest_offset,
            length,
            ..
        } => {
            assert_eq!(*guest_offset, block_size - 512);
            assert_eq!(*length, 512);
        }
        _ => unreachable!(),
    }
    // Second Data: first 512 bytes of block 1.
    match data_ranges[1] {
        WriteRange::Data {
            guest_offset,
            length,
            ..
        } => {
            assert_eq!(*guest_offset, block_size);
            assert_eq!(*length, 512);
        }
        _ => unreachable!(),
    }
}

#[async_test]
async fn write_then_read_roundtrip(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    // Step 1: resolve_write to get file offsets.
    let mut write_ranges = Vec::new();
    let guard = vhdx.resolve_write(0, 512, &mut write_ranges).await.unwrap();

    // Step 2: Write actual data at the returned Data offsets.
    let pattern: Vec<u8> = (0..512u16).map(|i| (i % 256) as u8).collect();
    for wr in &write_ranges {
        match wr {
            WriteRange::Data {
                file_offset,
                length,
                ..
            } => {
                vhdx.file
                    .write_at(*file_offset, &pattern[..(*length as usize)])
                    .await
                    .unwrap();
            }
            WriteRange::Zero {
                file_offset,
                length,
            } => {
                let zeros = vec![0u8; *length as usize];
                vhdx.file.write_at(*file_offset, &zeros).await.unwrap();
            }
        }
    }

    // Step 3: complete via guard.
    guard.complete().await.unwrap();

    // Step 4: resolve_read at the same offset.
    let mut read_ranges = Vec::new();
    let _guard = vhdx.resolve_read(0, 512, &mut read_ranges).await.unwrap();

    // Should now be Data (block was allocated).
    assert_eq!(read_ranges.len(), 1);
    match &read_ranges[0] {
        ReadRange::Data {
            file_offset,
            length,
            ..
        } => {
            assert_eq!(*length, 512);
            let mut buf = vec![0u8; 512];
            vhdx.file.read_at(*file_offset, &mut buf).await.unwrap();
            assert_eq!(buf, pattern);
        }
        other => panic!("expected Data read range, got {:?}", other),
    }
}

#[async_test]
async fn write_partial_block(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    // Write 512 bytes at offset 4096 within block 0.
    let mut ranges = Vec::new();
    let _guard = vhdx.resolve_write(4096, 512, &mut ranges).await.unwrap();

    // With safe data (near-EOF or extension space), Zero padding is
    // skipped. Only expect the Data range.
    // Without safe data, we'd see: Zero(leading 4096), Data(512), Zero(trailing).
    assert!(!ranges.is_empty());
    // Find the Data range.
    let data_range = ranges
        .iter()
        .find(|r| matches!(r, WriteRange::Data { .. }))
        .expect("expected at least one Data range");
    match data_range {
        WriteRange::Data {
            guest_offset,
            length,
            ..
        } => {
            assert_eq!(*guest_offset, 4096);
            assert_eq!(*length, 512);
        }
        _ => unreachable!(),
    }
}

#[async_test]
async fn write_full_block(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    // Write exactly one full block (no padding needed).
    let mut ranges = Vec::new();
    let _guard = vhdx
        .resolve_write(0, format::DEFAULT_BLOCK_SIZE, &mut ranges)
        .await
        .unwrap();

    // Should be exactly one Data range — no zero padding.
    assert_eq!(ranges.len(), 1);
    match ranges[0] {
        WriteRange::Data {
            guest_offset,
            length,
            file_offset,
        } => {
            assert_eq!(guest_offset, 0);
            assert_eq!(length, format::DEFAULT_BLOCK_SIZE);
            assert!(file_offset > 0);
            assert_eq!(file_offset % MB1, 0);
        }
        _ => panic!("expected Data range, got {:?}", ranges[0]),
    }
}

#[async_test]
async fn write_zero_length(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    let mut ranges = Vec::new();
    let _guard = vhdx.resolve_write(0, 0, &mut ranges).await.unwrap();
    assert!(ranges.is_empty());
}

#[async_test]
async fn write_beyond_end_of_disk(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    let mut ranges = Vec::new();
    let result = vhdx
        .resolve_write(format::GB1 - 512, 1024, &mut ranges)
        .await;
    assert!(matches!(
        result,
        Err(VhdxIoError(VhdxIoErrorInner::BeyondEndOfDisk))
    ));
}

#[async_test]
async fn write_read_only() {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).read_only().await.unwrap();

    let mut ranges = Vec::new();
    let result = vhdx.resolve_write(0, 4096, &mut ranges).await;
    assert!(matches!(
        result,
        Err(VhdxIoError(VhdxIoErrorInner::ReadOnly))
    ));
}

#[async_test]
async fn write_large_spanning_many_blocks(driver: DefaultDriver) {
    // 4 MiB disk with 1 MiB blocks → 4 blocks.
    let file = InMemoryFile::new(0);
    let mut params = CreateParams {
        disk_size: 4 * MB1,
        block_size: MB1 as u32,
        ..Default::default()
    };
    create::create(&file, &mut params).await.unwrap();
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    // Write 3 MiB starting at offset 512 KiB (spans blocks 0,1,2,3).
    let start = MB1 / 2;
    let length = (3 * MB1) as u32;
    let mut ranges = Vec::new();
    let _guard = vhdx
        .resolve_write(start, length, &mut ranges)
        .await
        .unwrap();

    let data_ranges: Vec<_> = ranges
        .iter()
        .filter(|r| matches!(r, WriteRange::Data { .. }))
        .collect();
    // Should span 4 blocks: partial block 0, full block 1, full block 2, partial block 3.
    assert_eq!(data_ranges.len(), 4);

    // Verify guest offsets and lengths.
    let block_size = MB1;
    match data_ranges[0] {
        WriteRange::Data {
            guest_offset,
            length,
            ..
        } => {
            assert_eq!(*guest_offset, start);
            assert_eq!(*length as u64, block_size - start);
        }
        _ => unreachable!(),
    }
    match data_ranges[1] {
        WriteRange::Data {
            guest_offset,
            length,
            ..
        } => {
            assert_eq!(*guest_offset, block_size);
            assert_eq!(*length as u64, block_size);
        }
        _ => unreachable!(),
    }
    match data_ranges[2] {
        WriteRange::Data {
            guest_offset,
            length,
            ..
        } => {
            assert_eq!(*guest_offset, 2 * block_size);
            assert_eq!(*length as u64, block_size);
        }
        _ => unreachable!(),
    }
    match data_ranges[3] {
        WriteRange::Data {
            guest_offset,
            length,
            ..
        } => {
            assert_eq!(*guest_offset, 3 * block_size);
            assert_eq!(*length as u64, start); // remaining half of last block
        }
        _ => unreachable!(),
    }
}

#[async_test]
async fn first_write_updates_header(driver: DefaultDriver) {
    let (file, params) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    let original_data_guid = params.data_write_guid;
    assert_eq!(vhdx.data_write_guid(), original_data_guid);

    // Perform a write — this triggers enable_write_mode.
    let mut ranges = Vec::new();
    let _guard = vhdx.resolve_write(0, 512, &mut ranges).await.unwrap();

    // data_write_guid should have changed.
    let new_data_guid = vhdx.data_write_guid();
    assert_ne!(new_data_guid, original_data_guid);
    assert_ne!(new_data_guid, Guid::ZERO);
}

#[async_test]
async fn second_write_no_header_update(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    // First write — triggers header update.
    let mut ranges = Vec::new();
    let _guard = vhdx.resolve_write(0, 512, &mut ranges).await.unwrap();
    let guid_after_first = vhdx.data_write_guid();

    // Second write — should NOT update header again.
    let mut ranges2 = Vec::new();
    let _guard2 = vhdx.resolve_write(512, 512, &mut ranges2).await.unwrap();
    let guid_after_second = vhdx.data_write_guid();

    assert_eq!(guid_after_first, guid_after_second);
}

#[async_test]
async fn file_writable_only_does_not_change_data_guid(driver: DefaultDriver) {
    let (file, params) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    let original_data_guid = params.data_write_guid;

    // Enable FileWritable mode (metadata-only modification).
    vhdx.enable_write_mode(WriteMode::FileWritable)
        .await
        .unwrap();

    // data_write_guid should NOT have changed.
    assert_eq!(vhdx.data_write_guid(), original_data_guid);

    // But the write mode should be set (subsequent DataWritable will escalate).
    assert_eq!(
        vhdx.header_state.write_mode(),
        Some(WriteMode::FileWritable)
    );
}

// --- TFP mechanics, write integration, and error path tests ---

/// Interceptor with toggleable failure for mid-test fault injection.
struct ToggleableInterceptor {
    fail_writes: Arc<AtomicBool>,
    fail_set_file_size: Arc<AtomicBool>,
}

impl IoInterceptor for ToggleableInterceptor {
    fn before_write(&self, _offset: u64, _data: &[u8]) -> Result<(), std::io::Error> {
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("injected write failure"));
        }
        Ok(())
    }

    fn before_set_file_size(&self, _size: u64) -> Result<(), std::io::Error> {
        if self.fail_set_file_size.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("injected set_file_size failure"));
        }
        Ok(())
    }
}

#[async_test]
async fn resolve_write_sets_tfp_on_full_block(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size();

    let mut ranges = Vec::new();
    let _guard = vhdx
        .resolve_write(0, block_size, &mut ranges)
        .await
        .unwrap();

    // Full-block write should set TFP on block 0.
    let mapping = vhdx.bat.get_block_mapping(0);
    assert!(
        mapping.transitioning_to_fully_present(),
        "full-block resolve_write should set TFP"
    );
    assert!(
        mapping.file_megabyte() > 0,
        "allocated block should have non-zero file offset"
    );
}

#[async_test]
async fn resolve_write_no_tfp_on_partial_block(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    let mut ranges = Vec::new();
    let _guard = vhdx.resolve_write(0, 512, &mut ranges).await.unwrap();

    // Partial-block write should NOT set TFP — BAT committed immediately.
    let mapping = vhdx.bat.get_block_mapping(0);
    assert!(
        !mapping.transitioning_to_fully_present(),
        "partial-block resolve_write should not set TFP"
    );
    assert_eq!(
        mapping.bat_state(),
        BatEntryState::FullyPresent,
        "partial allocation should set FullyPresent immediately"
    );
}

#[async_test]
async fn write_read_roundtrip_multi_block(driver: DefaultDriver) {
    let file = InMemoryFile::new(0);
    let mut params = CreateParams {
        disk_size: 4 * MB1,
        block_size: MB1 as u32,
        ..Default::default()
    };
    create::create(&file, &mut params).await.unwrap();
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    let block_size = vhdx.block_size() as u64;
    // Write 2 full blocks starting at offset 0.
    let length = (2 * block_size) as u32;
    let mut write_ranges = Vec::new();
    let guard = vhdx
        .resolve_write(0, length, &mut write_ranges)
        .await
        .unwrap();

    // Write recognizable pattern to each Data range.
    for wr in &write_ranges {
        match wr {
            WriteRange::Data {
                guest_offset,
                length,
                file_offset,
            } => {
                let pattern: Vec<u8> = (0..*length)
                    .map(|i| ((guest_offset + i as u64) % 251) as u8)
                    .collect();
                vhdx.file.write_at(*file_offset, &pattern).await.unwrap();
            }
            WriteRange::Zero {
                file_offset,
                length,
            } => {
                let zeros = vec![0u8; *length as usize];
                vhdx.file.write_at(*file_offset, &zeros).await.unwrap();
            }
        }
    }
    guard.complete().await.unwrap();

    // Read back both blocks.
    let mut read_ranges = Vec::new();
    let _guard = vhdx
        .resolve_read(0, length, &mut read_ranges)
        .await
        .unwrap();

    for rr in &read_ranges {
        match rr {
            ReadRange::Data {
                guest_offset,
                length,
                file_offset,
            } => {
                let mut buf = vec![0u8; *length as usize];
                vhdx.file.read_at(*file_offset, &mut buf).await.unwrap();
                let expected: Vec<u8> = (0..*length)
                    .map(|i| ((guest_offset + i as u64) % 251) as u8)
                    .collect();
                assert_eq!(
                    buf, expected,
                    "data mismatch at guest offset {guest_offset}"
                );
            }
            ReadRange::Zero { .. } => {
                panic!("expected Data range after write, got Zero");
            }
            ReadRange::Unmapped { .. } => {
                panic!("expected Data range after write, got Unmapped");
            }
        }
    }
}

#[async_test]
async fn write_to_already_allocated_no_growth(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let regions = region::parse_region_tables(&file).await.unwrap();

    // Pre-allocate block 0 as FullyPresent at offset 100 MB.
    let entry = BatEntry::new()
        .with_state(BatEntryState::FullyPresent as u8)
        .with_file_offset_mb(100);
    file.write_at(regions.bat_offset, entry.as_bytes())
        .await
        .unwrap();

    // Ensure file is big enough to cover that offset.
    let needed_size = 100 * MB1 + format::DEFAULT_BLOCK_SIZE as u64;
    file.set_file_size(needed_size).await.unwrap();

    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let eof_before = vhdx.allocation_lock.lock().await.file_length;

    let mut ranges = Vec::new();
    let _guard = vhdx.resolve_write(0, 4096, &mut ranges).await.unwrap();

    // No new allocation should occur — verify file length unchanged.
    let eof_after = vhdx.allocation_lock.lock().await.file_length;
    assert_eq!(
        eof_before, eof_after,
        "eof should not change for existing block"
    );

    // Should point to the existing block.
    assert_eq!(ranges.len(), 1);
    match ranges[0] {
        WriteRange::Data { file_offset, .. } => {
            assert_eq!(file_offset, 100 * MB1);
        }
        _ => panic!("expected Data range"),
    }
}

#[async_test]
async fn write_flush_persists_bat(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    // Write and complete a full block.
    let block_size = vhdx.block_size();
    let mut ranges = Vec::new();
    let guard = vhdx
        .resolve_write(0, block_size, &mut ranges)
        .await
        .unwrap();
    guard.complete().await.unwrap();
    vhdx.flush().await.unwrap();

    // Snapshot immediately after flush — proves flush persisted the BAT.
    // Log GUID is still set, so reopen will do log replay.
    let snapshot = vhdx.file.snapshot();

    // Reopen from snapshot (log replay recovers the state).
    let recovered = InMemoryFile::from_snapshot(snapshot);
    let vhdx2 = VhdxFile::open(recovered).writable(&driver).await.unwrap();
    let mapping = vhdx2.bat.get_block_mapping(0);
    assert_eq!(
        mapping.bat_state(),
        BatEntryState::FullyPresent,
        "BAT should show FullyPresent after flush + reopen"
    );
    assert!(
        mapping.file_megabyte() > 0,
        "BAT should have non-zero offset after flush + reopen"
    );
}

#[async_test]
async fn complete_write_clears_tfp(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size();

    // resolve_write should set TFP.
    let mut ranges = Vec::new();
    let guard = vhdx
        .resolve_write(0, block_size, &mut ranges)
        .await
        .unwrap();

    {
        let mapping = vhdx.bat.get_block_mapping(0);
        assert!(mapping.transitioning_to_fully_present());
    }

    // guard.complete() should clear TFP.
    guard.complete().await.unwrap();

    {
        let mapping = vhdx.bat.get_block_mapping(0);
        assert!(
            !mapping.transitioning_to_fully_present(),
            "TFP should be cleared after complete_write"
        );
        assert_eq!(
            mapping.bat_state(),
            BatEntryState::FullyPresent,
            "block should be FullyPresent after complete"
        );
    }
}

#[async_test]
async fn complete_write_writes_bat_to_disk(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size();

    let mut ranges = Vec::new();
    let guard = vhdx
        .resolve_write(0, block_size, &mut ranges)
        .await
        .unwrap();

    // Get the allocated offset from in-memory BAT.
    let expected_mb = vhdx.bat.get_block_mapping(0).file_megabyte();

    guard.complete().await.unwrap();
    vhdx.flush().await.unwrap();

    // Snapshot after flush — proves complete + flush persisted the BAT.
    let snapshot = vhdx.file.snapshot();

    // Reopen from snapshot (log replay recovers the state).
    let recovered = InMemoryFile::from_snapshot(snapshot);
    let vhdx2 = VhdxFile::open(recovered).writable(&driver).await.unwrap();
    let mapping = vhdx2.bat.get_block_mapping(0);
    assert_eq!(
        mapping.bat_state(),
        BatEntryState::FullyPresent,
        "BAT should be FullyPresent after flush + reopen"
    );
    assert_eq!(
        mapping.file_megabyte(),
        expected_mb,
        "BAT file offset should match after flush + reopen"
    );
}

#[async_test]
async fn resolve_write_extends_file(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    let size_before = vhdx.file.file_size().await.unwrap();

    let mut ranges = Vec::new();
    let _guard = vhdx.resolve_write(0, 512, &mut ranges).await.unwrap();

    let size_after = vhdx.file.file_size().await.unwrap();
    assert!(
        size_after > size_before,
        "file should grow after allocating a new block \
             (before={size_before}, after={size_after})"
    );
}

#[async_test]
async fn abort_write_reverts_bat(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size();

    // resolve_write for a full block → sets TFP.
    let mut ranges = Vec::new();
    let guard = vhdx
        .resolve_write(0, block_size, &mut ranges)
        .await
        .unwrap();

    // Abort (drop guard without complete) → reverts in-memory BAT.
    drop(guard);

    // Block should be back to NotPresent with zero offset.
    let mapping = vhdx.bat.get_block_mapping(0);
    assert_eq!(mapping.bat_state(), BatEntryState::NotPresent);
    assert_eq!(mapping.file_offset(), 0);

    vhdx.close().await.unwrap();
}

#[async_test]
async fn abort_write_clears_tfp(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size();

    let mut ranges = Vec::new();
    let guard = vhdx
        .resolve_write(0, block_size, &mut ranges)
        .await
        .unwrap();

    // TFP should be set.
    {
        assert!(
            vhdx.bat
                .get_block_mapping(0)
                .transitioning_to_fully_present()
        );
    }

    // Abort (drop guard without complete).
    drop(guard);

    // TFP should be cleared and state reverted to NotPresent.
    {
        let mapping = vhdx.bat.get_block_mapping(0);
        assert!(
            !mapping.transitioning_to_fully_present(),
            "TFP should be cleared after abort"
        );
        assert_eq!(
            mapping.bat_state(),
            BatEntryState::NotPresent,
            "should revert to original NotPresent state"
        );
        assert_eq!(
            mapping.file_megabyte(),
            0,
            "should revert file_megabyte to 0"
        );
    }
}

#[async_test]
async fn abort_write_allows_subsequent_write(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size();

    // First write: allocate and abort.
    let mut ranges = Vec::new();
    let guard = vhdx
        .resolve_write(0, block_size, &mut ranges)
        .await
        .unwrap();
    drop(guard);

    // Second write: should succeed (no TFP blocking).
    let mut ranges2 = Vec::new();
    let guard2 = vhdx
        .resolve_write(0, block_size, &mut ranges2)
        .await
        .unwrap();
    guard2.complete().await.unwrap();

    // Block should be FullyPresent now.
    let mapping = vhdx.bat.get_block_mapping(0);
    assert_eq!(mapping.bat_state(), BatEntryState::FullyPresent);
    assert!(!mapping.transitioning_to_fully_present());
}

#[async_test]
async fn complete_write_notifies_on_cache_failure(driver: DefaultDriver) {
    // With write-back mode (no write-through), cache writes during
    // complete() only mark pages dirty in the cache. The actual disk
    // write happens on flush through the log task. So complete()
    // itself should succeed even with write failures enabled.
    let (orig_file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let data = orig_file.snapshot();

    let fail_writes = Arc::new(AtomicBool::new(false));
    let interceptor = Arc::new(ToggleableInterceptor {
        fail_writes: fail_writes.clone(),
        fail_set_file_size: Arc::new(AtomicBool::new(false)),
    });
    let file = InMemoryFile::with_interceptor(0, interceptor);
    file.write_at(0, &data).await.unwrap();

    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size();

    // resolve_write succeeds (writes for header update, set_file_size).
    let mut ranges = Vec::new();
    let guard = vhdx
        .resolve_write(0, block_size, &mut ranges)
        .await
        .unwrap();

    // Enable write failure.
    fail_writes.store(true, Ordering::SeqCst);

    // complete() should succeed — commit() is a no-op in write-back mode,
    // and dirty pages are marked in cache without file I/O.
    let result = guard.complete().await;
    assert!(
        result.is_ok(),
        "complete() should succeed in write-back mode even with write failures"
    );

    // TFP should be cleared and state set to FullyPresent.
    {
        let mapping = vhdx.bat.get_block_mapping(0);
        assert!(
            !mapping.transitioning_to_fully_present(),
            "TFP should be cleared after complete"
        );
        assert_eq!(
            mapping.bat_state(),
            BatEntryState::FullyPresent,
            "state should be FullyPresent after complete"
        );
    }

    // Re-enable writes.
    fail_writes.store(false, Ordering::SeqCst);

    // A subsequent resolve_write should work (not hang on TFP).
    let mut ranges2 = Vec::new();
    let _guard2 = vhdx
        .resolve_write(0, block_size, &mut ranges2)
        .await
        .unwrap();
}

#[async_test]
async fn resolve_write_error_reverts_tfp(driver: DefaultDriver) {
    // Create VHDX normally, then snapshot to new file with toggleable interceptor.
    let (orig_file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let data = orig_file.snapshot();

    let fail_set_file_size = Arc::new(AtomicBool::new(false));
    let interceptor = Arc::new(ToggleableInterceptor {
        fail_writes: Arc::new(AtomicBool::new(false)),
        fail_set_file_size: fail_set_file_size.clone(),
    });
    let file = InMemoryFile::with_interceptor(0, interceptor);
    file.write_at(0, &data).await.unwrap();

    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size();

    // Enable set_file_size failure.
    fail_set_file_size.store(true, Ordering::SeqCst);

    // resolve_write should fail when set_file_size fails during allocation.
    let mut ranges = Vec::new();
    let result = vhdx.resolve_write(0, block_size, &mut ranges).await;
    assert!(
        result.is_err(),
        "resolve_write should fail when set_file_size fails"
    );

    // TFP should be reverted.
    {
        let mapping = vhdx.bat.get_block_mapping(0);
        assert!(
            !mapping.transitioning_to_fully_present(),
            "TFP should be reverted on resolve_write error"
        );
    }

    // Disable failure, retry should succeed.
    fail_set_file_size.store(false, Ordering::SeqCst);

    let mut ranges2 = Vec::new();
    let _guard = vhdx
        .resolve_write(0, block_size, &mut ranges2)
        .await
        .unwrap();
}

/// Verify that a new allocation from near-EOF (safe data) omits zero
/// padding, while an allocation from the free pool does emit zero padding.
#[async_test]
async fn safe_data_skips_zero_padding(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size() as u64;

    // Step 1: Partial write to block 0 at guest_offset=0, len=512.
    // Allocation comes from near-EOF → SpaceState::Zero → no zero ranges.
    let mut ranges = Vec::new();
    let _guard = vhdx.resolve_write(0, 512, &mut ranges).await.unwrap();

    let zero_ranges: Vec<_> = ranges
        .iter()
        .filter(|r| matches!(r, WriteRange::Zero { .. }))
        .collect();
    assert!(
        zero_ranges.is_empty(),
        "near-EOF allocation should skip zero padding, but got {} Zero ranges",
        zero_ranges.len(),
    );

    // Extract the block base offset (block_offset=0 since guest_offset=0).
    let allocated_offset = match ranges[0] {
        WriteRange::Data { file_offset, .. } => file_offset,
        _ => panic!("expected Data range"),
    };

    // Step 2: Release the allocated space back to pool.
    // (Intentionally creating an inconsistency for testing purposes.)
    vhdx.free_space
        .release(allocated_offset, vhdx.block_size() as u32);

    // Step 3: Partial write to block 1 at block-aligned guest offset.
    // Should allocate from pool (unsafe data) → zero ranges emitted.
    let mut ranges2 = Vec::new();
    let _guard2 = vhdx
        .resolve_write(block_size, 512, &mut ranges2)
        .await
        .unwrap();

    let zero_ranges2: Vec<_> = ranges2
        .iter()
        .filter(|r| matches!(r, WriteRange::Zero { .. }))
        .collect();
    assert!(
        !zero_ranges2.is_empty(),
        "pool allocation should emit zero padding, but got 0 Zero ranges",
    );
}

// ---- Concurrent I/O stress tests ----

/// Wrapper around `InMemoryFile` that yields once on `set_file_size`.
///
/// `InMemoryFile`'s async methods are synchronous (return Ready
/// immediately), so `futures::join!` won't interleave two
/// `resolve_write` calls. This wrapper inserts a
/// `futures::pending!()` call inside `set_file_size`, creating a yield
/// point during `allocate_space` while the `allocation_lock` is held.
struct YieldingFile {
    inner: InMemoryFile,
}

impl AsyncFile for YieldingFile {
    type Buffer = Vec<u8>;

    fn alloc_buffer(&self, len: usize) -> Vec<u8> {
        self.inner.alloc_buffer(len)
    }

    async fn read_into(&self, offset: u64, buf: Vec<u8>) -> Result<Vec<u8>, std::io::Error> {
        self.inner.read_into(offset, buf).await
    }

    async fn write_from(
        &self,
        offset: u64,
        buf: impl Borrow<Vec<u8>> + Send + 'static,
    ) -> Result<(), std::io::Error> {
        self.inner.write_from(offset, buf).await
    }

    async fn flush(&self) -> Result<(), std::io::Error> {
        self.inner.flush().await
    }
    async fn file_size(&self) -> Result<u64, std::io::Error> {
        self.inner.file_size().await
    }
    async fn set_file_size(&self, size: u64) -> Result<(), std::io::Error> {
        // Yield once to allow other futures to run, then resume.
        // We must wake ourselves before returning Pending, otherwise
        // the executor won't re-poll us (deadlock).
        let mut yielded = false;
        std::future::poll_fn(|cx| {
            if !yielded {
                yielded = true;
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            } else {
                std::task::Poll::Ready(())
            }
        })
        .await;
        self.inner.set_file_size(size).await
    }
}

/// Helper: create a VHDX with custom block size on an `InMemoryFile`,
/// returning the file and params.
async fn create_vhdx_with_block_size(
    disk_size: u64,
    block_size: u32,
) -> (InMemoryFile, CreateParams) {
    let file = InMemoryFile::new(0);
    let mut params = CreateParams {
        disk_size,
        block_size,
        ..Default::default()
    };
    create::create(&file, &mut params).await.unwrap();
    (file, params)
}

/// Helper: perform a full write-complete cycle on a single block.
async fn write_block<F: AsyncFile>(
    vhdx: &VhdxFile<F>,
    guest_offset: u64,
    length: u32,
    pattern_byte: u8,
) {
    let mut ranges = Vec::new();
    let guard = vhdx
        .resolve_write(guest_offset, length, &mut ranges)
        .await
        .unwrap();

    // Write pattern data at each Data range, zero at each Zero range.
    for wr in &ranges {
        match wr {
            WriteRange::Data {
                file_offset,
                length,
                ..
            } => {
                let data = vec![pattern_byte; *length as usize];
                vhdx.file.write_at(*file_offset, &data).await.unwrap();
            }
            WriteRange::Zero {
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

/// Helper: read a block and verify the pattern byte.
async fn verify_block_pattern<F: AsyncFile>(
    vhdx: &VhdxFile<F>,
    guest_offset: u64,
    length: u32,
    expected_byte: u8,
) {
    let mut ranges = Vec::new();
    let _guard = vhdx
        .resolve_read(guest_offset, length, &mut ranges)
        .await
        .unwrap();

    for rr in &ranges {
        match rr {
            ReadRange::Data {
                file_offset,
                length,
                ..
            } => {
                let mut buf = vec![0u8; *length as usize];
                vhdx.file.read_at(*file_offset, &mut buf).await.unwrap();
                assert!(
                    buf.iter().all(|&b| b == expected_byte),
                    "expected all bytes to be 0x{:02x} at file_offset {}, \
                         but found mismatch",
                    expected_byte,
                    file_offset,
                );
            }
            ReadRange::Zero { .. } => {
                assert_eq!(expected_byte, 0, "expected data but got Zero range");
            }
            ReadRange::Unmapped { .. } => {
                panic!("unexpected Unmapped range in non-differencing disk");
            }
        }
    }
}

#[async_test]
async fn concurrent_reads_same_block(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = Arc::new(VhdxFile::open(file).writable(&driver).await.unwrap());
    let block_size = vhdx.block_size();

    // Pre-allocate block 0 with known data.
    write_block(&*vhdx, 0, block_size, 0xAA).await;

    // Spawn 10 concurrent reads to the same block.
    let futures: Vec<_> = (0..10)
        .map(|_| {
            let vhdx = vhdx.clone();
            async move {
                let mut ranges = Vec::new();
                let _guard = vhdx.resolve_read(0, block_size, &mut ranges).await.unwrap();
                assert_eq!(ranges.len(), 1);
                match &ranges[0] {
                    ReadRange::Data {
                        guest_offset,
                        length,
                        file_offset,
                    } => {
                        assert_eq!(*guest_offset, 0);
                        assert_eq!(*length, block_size);
                        assert!(*file_offset > 0);
                    }
                    other => panic!("expected Data range, got {:?}", other),
                }
                ranges
            }
        })
        .collect();

    let results = futures::future::join_all(futures).await;

    // All results should be identical.
    let first = &results[0];
    for result in &results[1..] {
        assert_eq!(first, result);
    }
}

#[async_test]
async fn concurrent_reads_different_blocks(driver: DefaultDriver) {
    let (file, _) = create_vhdx_with_block_size(4 * MB1, MB1 as u32).await;
    let vhdx = Arc::new(VhdxFile::open(file).writable(&driver).await.unwrap());
    let block_size = vhdx.block_size();

    // Pre-allocate blocks 0, 1, 2.
    for i in 0..3u8 {
        write_block(&*vhdx, i as u64 * block_size as u64, block_size, 0x10 + i).await;
    }

    // Spawn 3 concurrent reads, one per block.
    let futures: Vec<_> = (0..3u32)
        .map(|i| {
            let vhdx = vhdx.clone();
            let bs = block_size;
            async move {
                let mut ranges = Vec::new();
                let _guard = vhdx
                    .resolve_read(i as u64 * bs as u64, bs, &mut ranges)
                    .await
                    .unwrap();
                assert_eq!(ranges.len(), 1);
                match &ranges[0] {
                    ReadRange::Data { file_offset, .. } => {
                        assert!(*file_offset > 0);
                    }
                    other => panic!("expected Data range for block {}, got {:?}", i, other),
                }
            }
        })
        .collect();

    futures::future::join_all(futures).await;
}

#[async_test]
async fn concurrent_writes_different_blocks(driver: DefaultDriver) {
    // 8 MiB disk with 1 MiB blocks → 8 blocks.
    let (file, _) = create_vhdx_with_block_size(8 * MB1, MB1 as u32).await;
    let vhdx = Arc::new(VhdxFile::open(file).writable(&driver).await.unwrap());
    let block_size = vhdx.block_size();

    // Spawn 4 concurrent tasks, each writing to a unique block.
    let futures: Vec<_> = (0..4u8)
        .map(|i| {
            let vhdx = vhdx.clone();
            let bs = block_size;
            async move {
                let offset = i as u64 * bs as u64;
                let pattern = 0x40 + i;
                write_block(&*vhdx, offset, bs, pattern).await;
            }
        })
        .collect();

    futures::future::join_all(futures).await;

    // Verify each block reads back the correct pattern.
    for i in 0..4u8 {
        let offset = i as u64 * block_size as u64;
        verify_block_pattern(&*vhdx, offset, block_size, 0x40 + i).await;
    }
}

#[async_test]
async fn concurrent_writes_same_block(driver: DefaultDriver) {
    // This test exercises concurrent writes to the same unallocated block.
    // The correct behavior is serialization:
    //   1. task_a: resolve_write → acquires allocation lock → allocates
    //      → sets TFP → returns ranges
    //   2. task_a: complete_write → clears TFP → FullyPresent → notifies
    //   3. task_b: resolve_write → was waiting for TFP to clear (either
    //      in the read phase or after acquiring the lock). Once cleared,
    //      sees FullyPresent → emits Data range → returns.
    //
    // Uses YieldingFile to force a yield during set_file_size (inside
    // allocate_space), creating the interleaving where task_b's read
    // phase may see NotPresent before task_a sets TFP.

    let (inner_file, _) = create_vhdx_with_block_size(4 * MB1, MB1 as u32).await;
    let data = inner_file.snapshot();

    let yielding_file = YieldingFile {
        inner: InMemoryFile::new(0),
    };
    yielding_file.inner.write_at(0, &data).await.unwrap();

    let vhdx = Arc::new(
        VhdxFile::open(yielding_file)
            .writable(&driver)
            .await
            .unwrap(),
    );
    let block_size = vhdx.block_size();

    // Both tasks write to block 0 (offset 0, full block).
    // task_a does resolve + complete as a unit so TFP clears and
    // task_b (serialized behind task_a) can proceed.
    let vhdx_a = vhdx.clone();
    let vhdx_b = vhdx.clone();

    let task_a = async {
        let mut ranges = Vec::new();
        let guard = vhdx_a
            .resolve_write(0, block_size, &mut ranges)
            .await
            .unwrap();
        guard.complete().await.unwrap();
        ranges
    };

    let task_b = async {
        let mut ranges = Vec::new();
        let _guard = vhdx_b
            .resolve_write(0, block_size, &mut ranges)
            .await
            .unwrap();
        ranges
    };

    let (ranges_a, ranges_b) = futures::join!(task_a, task_b);

    // Both should have produced data ranges.
    assert!(!ranges_a.is_empty(), "task_a produced no ranges");
    assert!(!ranges_b.is_empty(), "task_b produced no ranges");

    // Block should be FullyPresent.
    let mapping = vhdx.bat.get_block_mapping(0);
    assert_eq!(mapping.bat_state(), BatEntryState::FullyPresent);
    assert!(!mapping.transitioning_to_fully_present());
}

#[async_test]
async fn concurrent_flush_requests(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = Arc::new(VhdxFile::open(file).writable(&driver).await.unwrap());
    let block_size = vhdx.block_size();

    // Write to a block, complete.
    write_block(&*vhdx, 0, block_size, 0xBB).await;

    // Spawn 5 concurrent flush calls.
    let futures: Vec<_> = (0..5)
        .map(|_| {
            let vhdx = vhdx.clone();
            async move {
                vhdx.flush().await.unwrap();
            }
        })
        .collect();

    futures::future::join_all(futures).await;
}

#[async_test]
async fn stress_random_writes_no_corruption(driver: DefaultDriver) {
    // 8 MiB disk with 1 MiB blocks → 8 blocks.
    let (file, _) = create_vhdx_with_block_size(8 * MB1, MB1 as u32).await;
    let vhdx = Arc::new(VhdxFile::open(file).writable(&driver).await.unwrap());
    let block_size = vhdx.block_size();

    // Spawn 8 tasks, each claiming a unique block.
    let futures: Vec<_> = (0..8u8)
        .map(|i| {
            let vhdx = vhdx.clone();
            let bs = block_size;
            async move {
                let offset = i as u64 * bs as u64;
                let pattern = 0x80 + i;
                write_block(&*vhdx, offset, bs, pattern).await;
                vhdx.flush().await.unwrap();
            }
        })
        .collect();

    futures::future::join_all(futures).await;

    // Verify all blocks.
    for i in 0..8u8 {
        let offset = i as u64 * block_size as u64;
        verify_block_pattern(&*vhdx, offset, block_size, 0x80 + i).await;
    }
}

#[async_test]
async fn concurrent_read_and_write_same_block(driver: DefaultDriver) {
    let (file, _) = create_vhdx_with_block_size(4 * MB1, MB1 as u32).await;
    let vhdx = Arc::new(VhdxFile::open(file).writable(&driver).await.unwrap());
    let block_size = vhdx.block_size();

    // Pre-allocate block 0 with known data.
    write_block(&*vhdx, 0, block_size, 0xCC).await;

    // Concurrent: read block 0, write block 1.
    let vhdx_r = vhdx.clone();
    let vhdx_w = vhdx.clone();

    let read_task = async move {
        let mut ranges = Vec::new();
        let _guard = vhdx_r
            .resolve_read(0, block_size, &mut ranges)
            .await
            .unwrap();
        assert_eq!(ranges.len(), 1);
        match &ranges[0] {
            ReadRange::Data { .. } => {}
            other => panic!("expected Data range, got {:?}", other),
        }
    };

    let write_task = async move {
        let offset = block_size as u64;
        write_block(&*vhdx_w, offset, block_size, 0xDD).await;
    };

    futures::join!(read_task, write_task);

    // Verify block 0 still has original data.
    verify_block_pattern(&*vhdx, 0, block_size, 0xCC).await;
    // Verify block 1 has new data.
    verify_block_pattern(&*vhdx, block_size as u64, block_size, 0xDD).await;
}

// ---- IoGuard refcount tracking tests ----

#[async_test]
async fn read_guard_increments_refcount(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size();

    // Pre-allocate block 0 so it's FullyPresent.
    write_block(&vhdx, 0, block_size, 0xAA).await;

    // Resolve a read — refcount should be 1 while guard is alive.
    let mut ranges = Vec::new();
    let guard = vhdx.resolve_read(0, 4096, &mut ranges).await.unwrap();

    assert_eq!(vhdx.bat.io_refcount(0), 1);

    // Drop the guard — refcount should go back to 0.
    drop(guard);

    assert_eq!(vhdx.bat.io_refcount(0), 0);
}

#[async_test]
async fn read_guard_drop_decrements_refcount(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size();

    // Pre-allocate block 0.
    write_block(&vhdx, 0, block_size, 0xBB).await;

    let mut ranges = Vec::new();
    let guard = vhdx.resolve_read(0, block_size, &mut ranges).await.unwrap();

    // Refcount is 1 while guard is held.
    assert_eq!(vhdx.bat.io_refcount(0), 1);

    // Drop explicitly.
    drop(guard);

    // Refcount back to 0.
    assert_eq!(vhdx.bat.io_refcount(0), 0);
}

#[async_test]
async fn read_guard_multiple_blocks(driver: DefaultDriver) {
    let (file, _) = create_vhdx_with_block_size(4 * MB1, MB1 as u32).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size();

    // Write 3 blocks.
    write_block(&vhdx, 0, block_size, 0x11).await;
    write_block(&vhdx, block_size as u64, block_size, 0x22).await;
    write_block(&vhdx, 2 * block_size as u64, block_size, 0x33).await;

    // Read spanning all 3 blocks.
    let mut ranges = Vec::new();
    let guard = vhdx
        .resolve_read(0, 3 * block_size, &mut ranges)
        .await
        .unwrap();

    assert_eq!(vhdx.bat.io_refcount(0), 1);
    assert_eq!(vhdx.bat.io_refcount(1), 1);
    assert_eq!(vhdx.bat.io_refcount(2), 1);

    drop(guard);

    assert_eq!(vhdx.bat.io_refcount(0), 0);
    assert_eq!(vhdx.bat.io_refcount(1), 0);
    assert_eq!(vhdx.bat.io_refcount(2), 0);
}

#[async_test]
async fn read_guard_zero_range_has_refcount() {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).read_only().await.unwrap();

    // Read an unallocated (Zero) block — refcount is still incremented
    // (harmless, since trim won't touch unallocated blocks).
    let mut ranges = Vec::new();
    let guard = vhdx.resolve_read(0, 4096, &mut ranges).await.unwrap();

    assert_eq!(vhdx.bat.io_refcount(0), 1);

    drop(guard);

    assert_eq!(vhdx.bat.io_refcount(0), 0);
}

#[async_test]
async fn write_guard_complete_drops_refcount(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size();

    let mut ranges = Vec::new();
    let guard = vhdx
        .resolve_write(0, block_size, &mut ranges)
        .await
        .unwrap();

    // Refcount should be 1.
    assert_eq!(vhdx.bat.io_refcount(0), 1);

    // Write data and complete.
    for wr in &ranges {
        match wr {
            WriteRange::Data {
                file_offset,
                length,
                ..
            } => {
                let data = vec![0xEE; *length as usize];
                vhdx.file.write_at(*file_offset, &data).await.unwrap();
            }
            WriteRange::Zero {
                file_offset,
                length,
            } => {
                let zeros = vec![0u8; *length as usize];
                vhdx.file.write_at(*file_offset, &zeros).await.unwrap();
            }
        }
    }

    guard.complete().await.unwrap();

    // After complete + drop, refcount should be 0 and block should be FullyPresent.
    assert_eq!(vhdx.bat.io_refcount(0), 0);
    let mapping = vhdx.bat.get_block_mapping(0);
    assert_eq!(mapping.bat_state(), BatEntryState::FullyPresent);
}

#[async_test]
async fn write_guard_drop_aborts_and_decrements_refcount(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size();

    let mut ranges = Vec::new();
    let guard = vhdx
        .resolve_write(0, block_size, &mut ranges)
        .await
        .unwrap();

    // Refcount should be 1.
    assert_eq!(vhdx.bat.io_refcount(0), 1);

    // Drop without calling complete() — abort.
    drop(guard);

    // Refcount should be 0, block should be back to NotPresent.
    assert_eq!(vhdx.bat.io_refcount(0), 0);
    let mapping = vhdx.bat.get_block_mapping(0);
    assert_eq!(mapping.bat_state(), BatEntryState::NotPresent);
}

#[async_test]
async fn concurrent_read_guards_same_block(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size();

    // Pre-allocate block 0.
    write_block(&vhdx, 0, block_size, 0xFF).await;

    // Two concurrent reads on the same block.
    let mut ranges1 = Vec::new();
    let mut ranges2 = Vec::new();
    let guard1 = vhdx.resolve_read(0, 4096, &mut ranges1).await.unwrap();
    let guard2 = vhdx.resolve_read(0, 4096, &mut ranges2).await.unwrap();

    // Refcount should be 2.
    assert_eq!(vhdx.bat.io_refcount(0), 2);

    // Drop first guard — refcount should be 1.
    drop(guard1);
    assert_eq!(vhdx.bat.io_refcount(0), 1);

    // Drop second guard — refcount should be 0.
    drop(guard2);
    assert_eq!(vhdx.bat.io_refcount(0), 0);
}

// ---- Concurrent write+trim and mixed-workload stress tests ----

use crate::trim::TrimMode;
use crate::trim::TrimRequest;

#[async_test]
async fn concurrent_write_and_trim_same_block(driver: DefaultDriver) {
    // Setup: 8 MiB disk, 1 MiB blocks.
    let (file, _) = create_vhdx_with_block_size(8 * MB1, MB1 as u32).await;
    let vhdx = Arc::new(VhdxFile::open(file).writable(&driver).await.unwrap());
    let block_size = vhdx.block_size();

    // Write block 0 with pattern 0xAA, complete.
    write_block(&*vhdx, 0, block_size, 0xAA).await;

    // Concurrently: write block 0 with 0xEE + trim block 0 (FileSpace).
    let vhdx_w = vhdx.clone();
    let vhdx_t = vhdx.clone();

    let (write_result, trim_result) = futures::join!(
        async {
            write_block(&*vhdx_w, 0, block_size, 0xEE).await;
            Ok::<(), VhdxIoError>(())
        },
        async {
            vhdx_t
                .trim(TrimRequest::new(TrimMode::FileSpace, 0, block_size as u64))
                .await
        }
    );

    write_result.unwrap();
    trim_result.unwrap();

    // Check what actually happened by examining block state.
    let mapping = vhdx.bat.get_block_mapping(0);
    match mapping.bat_state() {
        BatEntryState::Unmapped => {
            // Trim won — read should return zeros.
            verify_block_pattern(&*vhdx, 0, block_size, 0x00).await;
        }
        BatEntryState::FullyPresent => {
            // Write won — read should return 0xEE.
            verify_block_pattern(&*vhdx, 0, block_size, 0xEE).await;
        }
        other => panic!("unexpected state: {other:?}"),
    }
}

#[async_test]
async fn concurrent_trim_then_rewrite(driver: DefaultDriver) {
    // Setup: 8 MiB disk, 1 MiB blocks.
    let (file, _) = create_vhdx_with_block_size(8 * MB1, MB1 as u32).await;
    let vhdx = Arc::new(VhdxFile::open(file).writable(&driver).await.unwrap());
    let block_size = vhdx.block_size();

    // Write block 0 with pattern 0xAA.
    write_block(&*vhdx, 0, block_size, 0xAA).await;

    // Sequential: trim → rewrite. Verify the trim→re-allocate path.
    vhdx.trim(TrimRequest::new(TrimMode::FileSpace, 0, block_size as u64))
        .await
        .unwrap();

    let mapping = vhdx.bat.get_block_mapping(0);
    assert_eq!(
        mapping.bat_state(),
        BatEntryState::Unmapped,
        "block should be Unmapped after trim"
    );

    // Re-write with pattern 0xBB.
    write_block(&*vhdx, 0, block_size, 0xBB).await;

    let mapping = vhdx.bat.get_block_mapping(0);
    assert_eq!(mapping.bat_state(), BatEntryState::FullyPresent);
    verify_block_pattern(&*vhdx, 0, block_size, 0xBB).await;

    // Now do trim + write concurrently.
    let vhdx_t = vhdx.clone();
    let vhdx_w = vhdx.clone();

    let (trim_result, write_result) = futures::join!(
        async {
            vhdx_t
                .trim(TrimRequest::new(TrimMode::FileSpace, 0, block_size as u64))
                .await
        },
        async {
            write_block(&*vhdx_w, 0, block_size, 0xCC).await;
            Ok::<(), VhdxIoError>(())
        }
    );

    trim_result.unwrap();
    write_result.unwrap();

    // Verify no panics and data is consistent.
    let mapping = vhdx.bat.get_block_mapping(0);
    match mapping.bat_state() {
        BatEntryState::Unmapped => {
            verify_block_pattern(&*vhdx, 0, block_size, 0x00).await;
        }
        BatEntryState::FullyPresent => {
            verify_block_pattern(&*vhdx, 0, block_size, 0xCC).await;
        }
        other => panic!("unexpected state: {other:?}"),
    }
}

#[async_test]
async fn mixed_workload_stress(driver: DefaultDriver) {
    // 8 MiB disk with 1 MiB blocks → 8 blocks.
    let (file, _) = create_vhdx_with_block_size(8 * MB1, MB1 as u32).await;
    let vhdx = Arc::new(VhdxFile::open(file).writable(&driver).await.unwrap());
    let block_size = vhdx.block_size();
    let num_blocks: u32 = 8;

    // Shadow state: None = unwritten/trimmed (expect zeros), Some(pattern) = last written pattern.
    let shadow: Arc<parking_lot::Mutex<Vec<Option<u8>>>> =
        Arc::new(parking_lot::Mutex::new(vec![None; num_blocks as usize]));

    let num_tasks: u32 = 8;
    let iters_per_task: u8 = 16;

    let tasks: Vec<_> = (0..num_tasks)
        .map(|task_id| {
            let vhdx = vhdx.clone();
            let shadow = shadow.clone();
            let bs = block_size;

            async move {
                for iter in 0..iters_per_task {
                    let block = (task_id.wrapping_mul(3).wrapping_add(iter as u32)) % num_blocks;
                    let pattern = ((task_id as u16 * 16 + iter as u16) as u8) | 0x01; // always nonzero
                    let block_offset = block as u64 * bs as u64;

                    let op = (task_id as u8).wrapping_add(iter) % 10;
                    match op {
                        0..=4 => {
                            // Write (50%)
                            write_block(&*vhdx, block_offset, bs, pattern).await;
                            shadow.lock()[block as usize] = Some(pattern);
                        }
                        5..=7 => {
                            // Read + verify (30%)
                            let expected = shadow.lock()[block as usize];
                            let mut ranges = Vec::new();
                            let guard = vhdx
                                .resolve_read(block_offset, bs, &mut ranges)
                                .await
                                .unwrap();
                            for rr in &ranges {
                                match rr {
                                    ReadRange::Data {
                                        file_offset,
                                        length,
                                        ..
                                    } => {
                                        let mut buf = vec![0u8; *length as usize];
                                        vhdx.file.read_at(*file_offset, &mut buf).await.unwrap();
                                        let exp = expected.unwrap_or_else(|| {
                                            panic!(
                                                "task {task_id} iter {iter}: shadow says \
                                                     None but got Data range"
                                            )
                                        });
                                        assert!(
                                            buf.iter().all(|&b| b == exp),
                                            "task {task_id} iter {iter}: expected \
                                                 0x{exp:02x}, got mismatch"
                                        );
                                    }
                                    ReadRange::Zero { .. } => {
                                        assert!(
                                            expected.is_none(),
                                            "task {task_id} iter {iter}: got Zero but \
                                                 expected Some({:02x})",
                                            expected.unwrap()
                                        );
                                    }
                                    ReadRange::Unmapped { .. } => {
                                        panic!("unexpected Unmapped on non-differencing disk");
                                    }
                                }
                            }
                            drop(guard);
                        }
                        8 => {
                            // Trim (10%)
                            vhdx.trim(TrimRequest::new(
                                TrimMode::FileSpace,
                                block_offset,
                                bs as u64,
                            ))
                            .await
                            .unwrap();
                            shadow.lock()[block as usize] = None;
                        }
                        9 => {
                            // Flush (10%)
                            vhdx.flush().await.unwrap();
                        }
                        _ => unreachable!(),
                    }
                }
            }
        })
        .collect();

    futures::future::join_all(tasks).await;

    // Post-check: verify every block against final shadow state.
    let final_shadow = shadow.lock().clone();
    for block in 0..num_blocks {
        let block_offset = block as u64 * block_size as u64;
        let expected = final_shadow[block as usize];
        match expected {
            Some(pattern) => {
                verify_block_pattern(&*vhdx, block_offset, block_size, pattern).await;
            }
            None => {
                // Should be zeros.
                let mut ranges = Vec::new();
                let _guard = vhdx
                    .resolve_read(block_offset, block_size, &mut ranges)
                    .await
                    .unwrap();
                for rr in &ranges {
                    match rr {
                        ReadRange::Zero { .. } => {}
                        ReadRange::Data {
                            file_offset,
                            length,
                            ..
                        } => {
                            let mut buf = vec![0u8; *length as usize];
                            vhdx.file.read_at(*file_offset, &mut buf).await.unwrap();
                            assert!(
                                buf.iter().all(|&b| b == 0),
                                "block {block}: shadow says None but data is non-zero"
                            );
                        }
                        ReadRange::Unmapped { .. } => {
                            panic!("unexpected Unmapped on non-differencing disk");
                        }
                    }
                }
            }
        }
    }
}

#[async_test]
async fn concurrent_partial_writes_same_block(driver: DefaultDriver) {
    // 8 MiB disk, 1 MiB blocks.
    let (file, _) = create_vhdx_with_block_size(8 * MB1, MB1 as u32).await;
    let vhdx = Arc::new(VhdxFile::open(file).writable(&driver).await.unwrap());
    let block_size = vhdx.block_size();

    // Pre-allocate block 0 with pattern 0xAA.
    write_block(&*vhdx, 0, block_size, 0xAA).await;

    let half = block_size / 2;
    let vhdx_a = vhdx.clone();
    let vhdx_b = vhdx.clone();

    // Concurrently write first half with 0xBB, second half with 0xCC.
    let ((), ()) = futures::join!(
        async {
            // Task A: write first half.
            let mut ranges = Vec::new();
            let guard = vhdx_a.resolve_write(0, half, &mut ranges).await.unwrap();
            for wr in &ranges {
                match wr {
                    WriteRange::Data {
                        file_offset,
                        length,
                        ..
                    } => {
                        let data = vec![0xBB; *length as usize];
                        vhdx_a.file.write_at(*file_offset, &data).await.unwrap();
                    }
                    WriteRange::Zero {
                        file_offset,
                        length,
                    } => {
                        let zeros = vec![0u8; *length as usize];
                        vhdx_a.file.write_at(*file_offset, &zeros).await.unwrap();
                    }
                }
            }
            guard.complete().await.unwrap();
        },
        async {
            // Task B: write second half.
            let mut ranges = Vec::new();
            let guard = vhdx_b
                .resolve_write(half as u64, half, &mut ranges)
                .await
                .unwrap();
            for wr in &ranges {
                match wr {
                    WriteRange::Data {
                        file_offset,
                        length,
                        ..
                    } => {
                        let data = vec![0xCC; *length as usize];
                        vhdx_b.file.write_at(*file_offset, &data).await.unwrap();
                    }
                    WriteRange::Zero {
                        file_offset,
                        length,
                    } => {
                        let zeros = vec![0u8; *length as usize];
                        vhdx_b.file.write_at(*file_offset, &zeros).await.unwrap();
                    }
                }
            }
            guard.complete().await.unwrap();
        }
    );

    // Read back full block: first half should be 0xBB, second half 0xCC.
    let mut ranges = Vec::new();
    let _guard = vhdx.resolve_read(0, block_size, &mut ranges).await.unwrap();

    for rr in &ranges {
        match rr {
            ReadRange::Data {
                guest_offset,
                length,
                file_offset,
            } => {
                let mut buf = vec![0u8; *length as usize];
                vhdx.file.read_at(*file_offset, &mut buf).await.unwrap();

                // Determine expected pattern based on position within block.
                for (i, &byte) in buf.iter().enumerate() {
                    let pos = (*guest_offset as usize) + i;
                    let expected = if pos < half as usize { 0xBB } else { 0xCC };
                    assert_eq!(
                        byte, expected,
                        "byte at guest offset {pos}: expected 0x{expected:02x}, got 0x{byte:02x}"
                    );
                }
            }
            other => panic!("expected Data range, got {other:?}"),
        }
    }
}

#[async_test]
async fn concurrent_write_flush_trim_interleaved(driver: DefaultDriver) {
    // Setup: 8 MiB disk, 1 MiB blocks.
    let (file, _) = create_vhdx_with_block_size(8 * MB1, MB1 as u32).await;
    let vhdx = Arc::new(VhdxFile::open(file).writable(&driver).await.unwrap());
    let block_size = vhdx.block_size();

    // Write block 0 with 0xDD, complete.
    write_block(&*vhdx, 0, block_size, 0xDD).await;

    let vhdx_f = vhdx.clone();
    let vhdx_t = vhdx.clone();
    let vhdx_r = vhdx.clone();

    // Concurrently: flush + trim block 0 + read block 1 (unallocated → zeros).
    let (flush_result, trim_result, read_result) = futures::join!(
        async { vhdx_f.flush().await },
        async {
            vhdx_t
                .trim(TrimRequest::new(TrimMode::FileSpace, 0, block_size as u64))
                .await
        },
        async {
            let mut ranges = Vec::new();
            let _guard = vhdx_r
                .resolve_read(block_size as u64, block_size, &mut ranges)
                .await
                .unwrap();
            // Block 1 is unallocated → should be Zero.
            for rr in &ranges {
                assert!(
                    matches!(rr, ReadRange::Zero { .. }),
                    "block 1 should be Zero, got {rr:?}"
                );
            }
            Ok::<(), VhdxIoError>(())
        }
    );

    flush_result.unwrap();
    trim_result.unwrap();
    read_result.unwrap();

    // Verify block 0 state is consistent.
    let mapping = vhdx.bat.get_block_mapping(0);
    match mapping.bat_state() {
        BatEntryState::Unmapped => {
            // Trim completed — read should return zeros.
            verify_block_pattern(&*vhdx, 0, block_size, 0x00).await;
        }
        BatEntryState::FullyPresent => {
            // Flush completed before trim could run — data preserved.
            verify_block_pattern(&*vhdx, 0, block_size, 0xDD).await;
        }
        other => panic!("unexpected state: {other:?}"),
    }
}

#[async_test]
async fn stress_write_trim_cycle(driver: DefaultDriver) {
    // 8 MiB disk with 1 MiB blocks → 8 blocks.
    let (file, _) = create_vhdx_with_block_size(8 * MB1, MB1 as u32).await;
    let vhdx = Arc::new(VhdxFile::open(file).writable(&driver).await.unwrap());
    let block_size = vhdx.block_size();

    let num_writer_tasks: u32 = 4;
    let num_reader_tasks: u32 = 2;
    let iters_per_writer: u8 = 8;

    // Shadow state: None = unwritten/trimmed (zeros), Some(pattern) = last written.
    let shadow: Arc<parking_lot::Mutex<Vec<Option<u8>>>> =
        Arc::new(parking_lot::Mutex::new(vec![None; 4]));

    // Writer tasks: write → trim → write again on block `task_id`.
    let writer_tasks: Vec<_> = (0..num_writer_tasks)
        .map(|task_id| {
            let vhdx = vhdx.clone();
            let shadow = shadow.clone();
            let bs = block_size;

            async move {
                for iter in 0..iters_per_writer {
                    let block_offset = task_id as u64 * bs as u64;
                    let pattern_a = ((task_id as u16 * 32 + iter as u16 * 2) as u8) | 0x01;
                    let pattern_b = ((task_id as u16 * 32 + iter as u16 * 2 + 1) as u8) | 0x01;

                    // Write with pattern_a.
                    write_block(&*vhdx, block_offset, bs, pattern_a).await;
                    shadow.lock()[task_id as usize] = Some(pattern_a);

                    // Trim.
                    vhdx.trim(TrimRequest::new(
                        TrimMode::FileSpace,
                        block_offset,
                        bs as u64,
                    ))
                    .await
                    .unwrap();
                    shadow.lock()[task_id as usize] = None;

                    // Write with pattern_b.
                    write_block(&*vhdx, block_offset, bs, pattern_b).await;
                    shadow.lock()[task_id as usize] = Some(pattern_b);
                }
            }
        })
        .collect();

    // Reader tasks: continuously read all 4 blocks, verify consistency.
    let reader_tasks: Vec<_> = (0..num_reader_tasks)
        .map(|_reader_id| {
            let vhdx = vhdx.clone();
            let shadow = shadow.clone();
            let bs = block_size;

            async move {
                // Read all 4 blocks multiple times.
                for _round in 0..16 {
                    for block in 0..4u32 {
                        let block_offset = block as u64 * bs as u64;
                        let expected = shadow.lock()[block as usize];

                        let mut ranges = Vec::new();
                        let guard = vhdx
                            .resolve_read(block_offset, bs, &mut ranges)
                            .await
                            .unwrap();
                        for rr in &ranges {
                            match rr {
                                ReadRange::Data {
                                    file_offset,
                                    length,
                                    ..
                                } => {
                                    let mut buf = vec![0u8; *length as usize];
                                    vhdx.file.read_at(*file_offset, &mut buf).await.unwrap();
                                    match expected {
                                        Some(exp) => {
                                            assert!(
                                                buf.iter().all(|&b| b == exp),
                                                "reader block {block}: expected \
                                                     0x{exp:02x}, got mismatch"
                                            );
                                        }
                                        None => {
                                            assert!(
                                                buf.iter().all(|&b| b == 0),
                                                "reader block {block}: expected zeros, \
                                                     got non-zero data"
                                            );
                                        }
                                    }
                                }
                                ReadRange::Zero { .. } => {
                                    assert!(
                                        expected.is_none(),
                                        "reader block {block}: got Zero but expected \
                                             Some({:02x})",
                                        expected.unwrap()
                                    );
                                }
                                ReadRange::Unmapped { .. } => {
                                    panic!("unexpected Unmapped on non-differencing disk");
                                }
                            }
                        }
                        drop(guard);
                    }
                }
            }
        })
        .collect();

    // Run all tasks concurrently.
    let all_tasks: Vec<_> = writer_tasks
        .into_iter()
        .map(|t| Box::pin(t) as std::pin::Pin<Box<dyn Future<Output = ()>>>)
        .chain(
            reader_tasks
                .into_iter()
                .map(|t| Box::pin(t) as std::pin::Pin<Box<dyn Future<Output = ()>>>),
        )
        .collect();

    futures::future::join_all(all_tasks).await;

    // Post-check: verify final state of all 4 blocks.
    let final_shadow = shadow.lock().clone();
    for block in 0..4u32 {
        let block_offset = block as u64 * block_size as u64;
        match final_shadow[block as usize] {
            Some(pattern) => {
                verify_block_pattern(&*vhdx, block_offset, block_size, pattern).await;
            }
            None => {
                verify_block_pattern(&*vhdx, block_offset, block_size, 0x00).await;
            }
        }
    }
}

// ---- SBM allocation tests ----

#[async_test]
async fn partial_write_diff_disk_allocates_sbm(driver: DefaultDriver) {
    // A sub-block write to a NotPresent block in a differencing disk
    // should allocate the SBM block and set the payload to PartiallyPresent.
    let file = InMemoryFile::new(0);
    let mut params = CreateParams {
        disk_size: format::GB1,
        has_parent: true,
        ..Default::default()
    };
    create::create(&file, &mut params).await.unwrap();
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    // Partial write: 4096 bytes at offset 0 (sub-block).
    write_block(&vhdx, 0, 4096, 0xAB).await;

    // Block 0 should be PartiallyPresent.
    let mapping = vhdx.bat.get_block_mapping(0);
    assert_eq!(mapping.bat_state(), BatEntryState::PartiallyPresent);

    // SBM block for chunk 0 should be FullyPresent (allocated).
    let sbm_mapping = vhdx.bat.get_sector_bitmap_mapping(0);
    assert_eq!(sbm_mapping.bat_state(), BatEntryState::FullyPresent);

    // Read the written range — should return Data.
    let mut ranges = Vec::new();
    let _guard = vhdx.resolve_read(0, 4096, &mut ranges).await.unwrap();
    let has_data = ranges.iter().any(|r| matches!(r, ReadRange::Data { .. }));
    assert!(has_data, "written sectors should return Data");

    // Read an unwritten range in the same block — should return Unmapped.
    let mut ranges2 = Vec::new();
    let _guard2 = vhdx.resolve_read(4096, 512, &mut ranges2).await.unwrap();
    assert_eq!(ranges2.len(), 1);
    assert!(
        matches!(ranges2[0], ReadRange::Unmapped { .. }),
        "unwritten sectors in diff disk should return Unmapped"
    );
}

#[async_test]
async fn partial_write_diff_disk_sbm_bits_set_correctly(driver: DefaultDriver) {
    // Write 4096 bytes (sectors 0-7 for 512-byte sectors) to a diff disk.
    // Verify that the written sectors read as Data and unwritten ones as Unmapped.
    let file = InMemoryFile::new(0);
    let mut params = CreateParams {
        disk_size: format::GB1,
        has_parent: true,
        ..Default::default()
    };
    create::create(&file, &mut params).await.unwrap();
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    write_block(&vhdx, 0, 4096, 0xCD).await;

    // Sectors 0-7 should be Data.
    let mut ranges = Vec::new();
    let _guard = vhdx.resolve_read(0, 4096, &mut ranges).await.unwrap();
    assert_eq!(ranges.len(), 1);
    match &ranges[0] {
        ReadRange::Data {
            guest_offset,
            length,
            ..
        } => {
            assert_eq!(*guest_offset, 0);
            assert_eq!(*length, 4096);
        }
        other => panic!("expected Data, got {:?}", other),
    }

    // Sector 8 onward should be Unmapped (transparent to parent).
    let mut ranges2 = Vec::new();
    let _guard2 = vhdx.resolve_read(4096, 512, &mut ranges2).await.unwrap();
    assert_eq!(ranges2.len(), 1);
    assert_eq!(
        ranges2[0],
        ReadRange::Unmapped {
            guest_offset: 4096,
            length: 512,
        }
    );
}

#[async_test]
async fn full_block_write_diff_disk_no_sbm(driver: DefaultDriver) {
    // A full-block write to a diff disk should set FullyPresent, not allocate SBM.
    let file = InMemoryFile::new(0);
    let mut params = CreateParams {
        disk_size: format::GB1,
        has_parent: true,
        ..Default::default()
    };
    create::create(&file, &mut params).await.unwrap();
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size();

    // Full-block write.
    write_block(&vhdx, 0, block_size, 0xEE).await;

    // Block 0 should be FullyPresent (TFP path).
    let mapping = vhdx.bat.get_block_mapping(0);
    assert_eq!(mapping.bat_state(), BatEntryState::FullyPresent);

    // SBM block for chunk 0 should NOT be allocated.
    let sbm_mapping = vhdx.bat.get_sector_bitmap_mapping(0);
    assert_ne!(
        sbm_mapping.bat_state(),
        BatEntryState::FullyPresent,
        "full-block write should not allocate SBM"
    );
}

#[async_test]
async fn second_partial_write_same_chunk_reuses_sbm(driver: DefaultDriver) {
    // Two partial writes to different blocks in the same chunk should
    // reuse the same SBM block.
    let file = InMemoryFile::new(0);
    let mut params = CreateParams {
        disk_size: format::GB1,
        has_parent: true,
        ..Default::default()
    };
    create::create(&file, &mut params).await.unwrap();
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size() as u64;

    // First partial write to block 0.
    write_block(&vhdx, 0, 4096, 0x11).await;

    let sbm_mapping_1 = vhdx.bat.get_sector_bitmap_mapping(0);
    assert_eq!(sbm_mapping_1.bat_state(), BatEntryState::FullyPresent);
    let sbm_offset_1 = sbm_mapping_1.file_offset();

    // Second partial write to block 1 (same chunk).
    write_block(&vhdx, block_size, 4096, 0x22).await;

    let sbm_mapping_2 = vhdx.bat.get_sector_bitmap_mapping(0);
    assert_eq!(sbm_mapping_2.bat_state(), BatEntryState::FullyPresent);
    let sbm_offset_2 = sbm_mapping_2.file_offset();

    // SBM should be reused (same file offset).
    assert_eq!(
        sbm_offset_1, sbm_offset_2,
        "SBM block should be reused, not reallocated"
    );

    // Both blocks should read back correctly.
    let mut ranges0 = Vec::new();
    let _g0 = vhdx.resolve_read(0, 4096, &mut ranges0).await.unwrap();
    assert!(matches!(ranges0[0], ReadRange::Data { .. }));

    let mut ranges1 = Vec::new();
    let _g1 = vhdx
        .resolve_read(block_size, 4096, &mut ranges1)
        .await
        .unwrap();
    assert!(matches!(ranges1[0], ReadRange::Data { .. }));
}

#[async_test]
async fn partial_write_non_diff_disk_no_sbm(driver: DefaultDriver) {
    // A sub-block write to a non-differencing disk should set FullyPresent
    // and NOT allocate any SBM block.
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

    // Partial write: 4096 bytes at offset 0.
    write_block(&vhdx, 0, 4096, 0x77).await;

    // Block should be FullyPresent (not PartiallyPresent).
    let mapping = vhdx.bat.get_block_mapping(0);
    assert_eq!(mapping.bat_state(), BatEntryState::FullyPresent);

    // SBM should NOT be allocated.
    // For non-diff disks, sector_bitmap_block_count may be 0,
    // so we check via bat_state directly.
    let sbm_count = vhdx.bat.sector_bitmap_block_count;
    if sbm_count > 0 {
        let sbm_mapping = vhdx.bat.get_sector_bitmap_mapping(0);
        assert_ne!(
            sbm_mapping.bat_state(),
            BatEntryState::FullyPresent,
            "non-diff disk should not allocate SBM"
        );
    }

    // Unwritten sectors within the block should read as Zero (not Unmapped).
    let mut ranges = Vec::new();
    let _guard = vhdx.resolve_read(4096, 512, &mut ranges).await.unwrap();
    assert_eq!(ranges.len(), 1);
    match &ranges[0] {
        ReadRange::Data { .. } => {
            // Data range is fine — zero-padded data within an allocated block.
        }
        ReadRange::Zero { .. } => {
            // Zero range is also acceptable (block may be zero-padded).
        }
        ReadRange::Unmapped { .. } => {
            panic!("non-diff disk should never return Unmapped for allocated block");
        }
    }
}

// -----------------------------------------------------------------------
// File poisoning tests
// -----------------------------------------------------------------------

/// Interceptor with atomic flags for runtime fault injection.
struct DynamicFailInterceptor {
    fail_writes: AtomicBool,
    fail_flushes: AtomicBool,
}

impl DynamicFailInterceptor {
    fn new() -> Self {
        Self {
            fail_writes: AtomicBool::new(false),
            fail_flushes: AtomicBool::new(false),
        }
    }
}

impl IoInterceptor for DynamicFailInterceptor {
    fn before_write(&self, _offset: u64, _data: &[u8]) -> Result<(), std::io::Error> {
        if self.fail_writes.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("injected write failure"));
        }
        Ok(())
    }

    fn before_flush(&self) -> Result<(), std::io::Error> {
        if self.fail_flushes.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("injected flush failure"));
        }
        Ok(())
    }
}

/// Helper: create a writable VHDX with a dynamic fault interceptor.
async fn create_writable_with_faults(
    driver: &DefaultDriver,
) -> (VhdxFile<InMemoryFile>, Arc<DynamicFailInterceptor>) {
    // Create a clean VHDX, snapshot it, reopen with interceptor.
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let snapshot = file.snapshot();

    let interceptor = Arc::new(DynamicFailInterceptor::new());
    let file2 = InMemoryFile::with_interceptor(snapshot.len() as u64, interceptor.clone());
    file2.write_at(0, &snapshot).await.unwrap();

    let vhdx = VhdxFile::open(file2).writable(driver).await.unwrap();
    (vhdx, interceptor)
}

#[async_test]
async fn flush_io_error_poisons_file(driver: DefaultDriver) {
    let (vhdx, interceptor) = create_writable_with_faults(&driver).await;

    // Write some data successfully.
    let data = [0xAAu8; 4096];
    let mut ranges = Vec::new();
    let guard = vhdx.resolve_write(0, 4096, &mut ranges).await.unwrap();
    for range in &ranges {
        if let WriteRange::Data {
            file_offset,
            length,
            ..
        } = range
        {
            vhdx.file
                .write_at(*file_offset, &data[..*length as usize])
                .await
                .unwrap();
        }
    }
    guard.complete().await.unwrap();

    // Now inject flush failure.
    interceptor.fail_flushes.store(true, Ordering::Relaxed);

    // Flush should fail.
    let result = vhdx.flush().await;
    assert!(result.is_err(), "flush should fail with injected error");

    // Disable the fault — shouldn't matter, file is poisoned.
    interceptor.fail_flushes.store(false, Ordering::Relaxed);

    // Subsequent writes should be rejected with Failed.
    {
        let mut ranges = Vec::new();
        let result = vhdx.resolve_write(0, 4096, &mut ranges).await;
        assert!(
            matches!(result, Err(VhdxIoError(VhdxIoErrorInner::Failed(_)))),
            "write after poison should return Failed"
        );
    }

    // Reads should also be rejected.
    {
        let mut ranges = Vec::new();
        let result = vhdx.resolve_read(0, 4096, &mut ranges).await;
        assert!(
            matches!(result, Err(VhdxIoError(VhdxIoErrorInner::Failed(_)))),
            "read after poison should return Failed"
        );
    }

    vhdx.abort().await;
}

#[async_test]
async fn apply_write_error_poisons_file(driver: DefaultDriver) {
    let (vhdx, interceptor) = create_writable_with_faults(&driver).await;

    // Write one block successfully and flush to ensure the pipeline works.
    let data = [0xBBu8; 4096];
    let mut ranges = Vec::new();
    let guard = vhdx.resolve_write(0, 4096, &mut ranges).await.unwrap();
    for range in &ranges {
        if let WriteRange::Data {
            file_offset,
            length,
            ..
        } = range
        {
            vhdx.file
                .write_at(*file_offset, &data[..*length as usize])
                .await
                .unwrap();
        }
    }
    guard.complete().await.unwrap();
    vhdx.flush().await.unwrap();

    // Now inject write failures — this will hit the log task when
    // it tries to write the WAL entry, and/or the apply task when
    // it tries to write pages to their final file offsets.
    interceptor.fail_writes.store(true, Ordering::Relaxed);

    // Write to a different block to generate new dirty BAT pages.
    let block_size = vhdx.block_size() as u64;
    let mut ranges = Vec::new();
    let guard = vhdx
        .resolve_write(block_size, 4096, &mut ranges)
        .await
        .unwrap();
    for range in &ranges {
        if let WriteRange::Data {
            file_offset,
            length,
            ..
        } = range
        {
            let _ = vhdx
                .file
                .write_at(*file_offset, &data[..*length as usize])
                .await;
        }
    }
    guard.complete().await.unwrap();

    // Flush sends to the log pipeline. The log task's WAL write
    // will hit the injected failure and poison the file.
    let _ = vhdx.flush().await;

    // Clear the fault — the file should stay poisoned regardless.
    interceptor.fail_writes.store(false, Ordering::Relaxed);

    // A second flush attempt synchronizes with the poisoned pipeline
    // and ensures the error has propagated.
    let _ = vhdx.flush().await;

    // The file should now be poisoned. Try an operation.
    {
        let mut ranges = Vec::new();
        let result = vhdx.resolve_write(0, 4096, &mut ranges).await;
        assert!(
            matches!(result, Err(VhdxIoError(VhdxIoErrorInner::Failed(_)))),
            "write after apply failure should return Failed"
        );
    }

    vhdx.abort().await;
}

#[async_test]
async fn poison_error_message_preserved(driver: DefaultDriver) {
    let (vhdx, interceptor) = create_writable_with_faults(&driver).await;

    // Write data.
    let data = [0xCCu8; 4096];
    let mut ranges = Vec::new();
    let guard = vhdx.resolve_write(0, 4096, &mut ranges).await.unwrap();
    for range in &ranges {
        if let WriteRange::Data {
            file_offset,
            length,
            ..
        } = range
        {
            vhdx.file
                .write_at(*file_offset, &data[..*length as usize])
                .await
                .unwrap();
        }
    }
    guard.complete().await.unwrap();

    // Inject flush failure and flush.
    interceptor.fail_flushes.store(true, Ordering::Relaxed);
    let _ = vhdx.flush().await;

    // The error message should contain something useful.
    let result = vhdx.failed.check();
    match result {
        Err(VhdxIoError(VhdxIoErrorInner::Failed(pf))) => {
            assert!(
                !pf.to_string().is_empty(),
                "poison error message should not be empty"
            );
        }
        other => panic!("expected Failed, got: {other:?}"),
    }

    vhdx.abort().await;
}

// ---- Post-Log Crash Consistency Tests ----
//
// These tests exercise crash recovery scenarios that aren't covered by
// the basic crash tests or concurrent tests. They focus on:
//   1. Unsafe (free-pool) allocation → flush → crash → no data teleportation
//   2. High-volume log pipeline saturation → crash → replay
//   3. Repeated crash-recovery cycles with writable reopen

use crate::tests::support::CrashTestFile;

/// Helper: write a data pattern via the write path.
async fn write_pattern_p16<F: AsyncFile>(vhdx: &VhdxFile<F>, offset: u64, len: usize, value: u8) {
    let write_buf = vec![value; len];
    let mut ranges = Vec::new();
    let guard = vhdx
        .resolve_write(offset, len as u32, &mut ranges)
        .await
        .unwrap();
    for range in &ranges {
        match range {
            WriteRange::Data {
                file_offset,
                length,
                ..
            } => {
                vhdx.file
                    .write_at(*file_offset, &write_buf[..(*length as usize)])
                    .await
                    .unwrap();
            }
            WriteRange::Zero {
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
async fn read_pattern_p16<F: AsyncFile>(vhdx: &VhdxFile<F>, offset: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let mut ranges = Vec::new();
    let _guard = vhdx
        .resolve_read(offset, len as u32, &mut ranges)
        .await
        .unwrap();
    for range in &ranges {
        match range {
            ReadRange::Data {
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
            ReadRange::Zero {
                guest_offset,
                length,
            } => {
                let start = (*guest_offset - offset) as usize;
                let end = start + *length as usize;
                buf[start..end].fill(0);
            }
            ReadRange::Unmapped { .. } => {}
        }
    }
    buf
}

/// Unsafe (free-pool) allocation → flush → crash → no data teleportation.
///
/// Allocate block A, trim it to the free pool, then write block B which
/// reuses A's freed space. Flush (so the WAL + FSN barrier are exercised),
/// then crash and replay. Verify:
///   - Block B has its own data (not A's old data)
///   - Block A reads as zeros (trimmed)
///   - No data from A "teleports" to B via stale on-disk content
///
/// This is the end-to-end crash test for the pre_log_fsn barrier mechanism.
/// Existing tests verify the barrier is *set* (bat_page_has_fsn_unsafe_free_pool)
/// and that a flush *occurs* (flush_between_data_and_wal_unsafe), but no
/// existing test verifies that data is correct after crash+replay when
/// the barrier was needed.
#[async_test]
async fn crash_unsafe_reuse_no_teleportation(driver: DefaultDriver) {
    let (mem_file, _) = create_vhdx_with_block_size(4 * MB1, MB1 as u32).await;
    let snapshot = mem_file.snapshot();

    let crash_file = CrashTestFile::from_durable(snapshot);
    let vhdx = VhdxFile::open(crash_file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size() as u64;

    // Step 1: Allocate block 0 with pattern 0xAA (near-EOF, safe).
    write_pattern_p16(&vhdx, 0, block_size as usize, 0xAA).await;
    vhdx.flush().await.unwrap();

    // Step 2: Trim block 0 with FreeSpace mode → releases to free pool.
    let trim_req = TrimRequest::new(TrimMode::FreeSpace, 0, block_size);
    vhdx.trim(trim_req).await.unwrap();
    vhdx.flush().await.unwrap();

    // Step 3: Write block 1 — should reuse block 0's freed space.
    // This is the unsafe allocation (SpaceState::CrossStale) that
    // requires a pre_log_fsn barrier.
    write_pattern_p16(&vhdx, block_size, block_size as usize, 0xBB).await;
    vhdx.flush().await.unwrap();

    // Crash.
    let durable = vhdx.file.durable_snapshot();
    vhdx.abort().await;

    // Recover and verify.
    let recovered = InMemoryFile::from_snapshot(durable);
    let vhdx2 = VhdxFile::open(recovered)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();

    // Block 0 should be zeros (trimmed with FreeSpace → Unmapped/Zero).
    let buf0 = read_pattern_p16(&vhdx2, 0, block_size as usize).await;
    assert!(
        buf0.iter().all(|&b| b == 0),
        "block 0 should be zeros after FreeSpace trim + crash"
    );

    // Block 1 should have 0xBB (not 0xAA — no teleportation).
    let buf1 = read_pattern_p16(&vhdx2, block_size, block_size as usize).await;
    assert!(
        buf1.iter().all(|&b| b == 0xBB),
        "block 1 should have 0xBB, not stale data from block 0"
    );
}

/// High-volume log pipeline stress + crash + replay.
///
/// Writes many blocks through the full commit→log→apply pipeline (enough
/// to trigger LogFull retry and circular buffer wrapping), then crashes
/// and replays. Verifies all flushed data survives and the log replays
/// correctly even after heavy use.
///
/// This combines the load profile of `log_pipeline_stress` (500 blocks)
/// with CrashTestFile crash semantics, which no existing test does.
#[async_test]
async fn crash_high_volume_pipeline(driver: DefaultDriver) {
    const BLOCK_COUNT: usize = 100;
    const BLOCK_SIZE: u64 = 2 * MB1;
    const WRITE_LEN: usize = 4096;

    let disk_size = BLOCK_SIZE * (BLOCK_COUNT as u64 + 1);
    let (mem_file, _) = create_vhdx_with_block_size(disk_size, BLOCK_SIZE as u32).await;
    let snapshot = mem_file.snapshot();

    let crash_file = CrashTestFile::from_durable(snapshot);
    let vhdx = VhdxFile::open(crash_file).writable(&driver).await.unwrap();

    // Write 100 distinct blocks. The cache will trigger batch-full commits
    // as dirty pages accumulate, and the log task will hit LogFull and
    // retry as the circular buffer fills.
    for i in 0..BLOCK_COUNT {
        let offset = i as u64 * BLOCK_SIZE;
        let pattern = (i & 0xFF) as u8;
        write_pattern_p16(&vhdx, offset, WRITE_LEN, pattern).await;
    }

    // Flush everything — drives all batches through commit→log→apply.
    vhdx.flush().await.unwrap();

    // Crash (no clean close — log_guid remains set).
    let durable = vhdx.file.durable_snapshot();
    vhdx.abort().await;

    // Recover with log replay.
    let recovered = InMemoryFile::from_snapshot(durable);
    let vhdx2 = VhdxFile::open(recovered)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();

    // Verify every block survived.
    for i in 0..BLOCK_COUNT {
        let offset = i as u64 * BLOCK_SIZE;
        let expected = (i & 0xFF) as u8;
        let buf = read_pattern_p16(&vhdx2, offset, WRITE_LEN).await;
        assert!(
            buf.iter().all(|&b| b == expected),
            "block {i}: expected 0x{expected:02X}, got 0x{:02X}",
            buf[0],
        );
    }
}

/// Repeated crash-recovery cycles with writable reopen.
///
/// Each cycle: open writable → write new data → flush → crash → verify.
/// The next cycle reopens writable from the crashed state. This tests
/// that log replay produces a file that can be opened writable again
/// (new log set up, new sequence numbers, etc.) without corruption
/// accumulating over multiple cycles.
///
/// Existing tests (`crash_recovery_then_more_writes`) do 2 rounds but
/// always reopen read-only for verification. This test reopens writable
/// for 5 consecutive cycles, verifying the full open-write-crash-recover
/// lifecycle.
#[async_test]
async fn crash_repeated_writable_recovery_cycles(driver: DefaultDriver) {
    const CYCLES: usize = 5;

    let (mem_file, _) = create_vhdx_with_block_size(8 * MB1, MB1 as u32).await;
    let mut durable = mem_file.snapshot();
    let block_size = MB1;

    for cycle in 0..CYCLES {
        // Open writable from the (possibly crashed) durable state.
        let crash_file = CrashTestFile::from_durable(durable);
        let vhdx = VhdxFile::open(crash_file).writable(&driver).await.unwrap();

        // Write to a different block each cycle.
        let offset = cycle as u64 * block_size;
        let pattern = (0x10 + cycle as u8) | 0x01; // nonzero
        write_pattern_p16(&vhdx, offset, block_size as usize, pattern).await;
        vhdx.flush().await.unwrap();

        // Verify all blocks from this and previous cycles are correct.
        for prev in 0..=cycle {
            let prev_offset = prev as u64 * block_size;
            let prev_pattern = (0x10 + prev as u8) | 0x01;
            let buf = read_pattern_p16(&vhdx, prev_offset, block_size as usize).await;
            assert!(
                buf.iter().all(|&b| b == prev_pattern),
                "cycle {cycle}, block {prev}: expected 0x{prev_pattern:02x}, \
                 got 0x{:02x}",
                buf[0]
            );
        }

        // Crash.
        durable = vhdx.file.durable_snapshot();
        vhdx.abort().await;
    }

    // Final verification: open read-only from the last crash, verify
    // all 5 blocks from all cycles survived.
    let recovered = InMemoryFile::from_snapshot(durable);
    let vhdx = VhdxFile::open(recovered)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();

    for cycle in 0..CYCLES {
        let offset = cycle as u64 * block_size;
        let pattern = (0x10 + cycle as u8) | 0x01;
        let buf = read_pattern_p16(&vhdx, offset, block_size as usize).await;
        assert!(
            buf.iter().all(|&b| b == pattern),
            "final verify block {cycle}: expected 0x{pattern:02x}, got 0x{:02x}",
            buf[0]
        );
    }
}

// ---- Concurrent crash tests using YieldingCrashFile ----
//
// These tests use YieldingCrashFile to create genuine interleaving between
// the log task, apply task, and user write tasks. The yield points cause
// the apply task to yield during write_at, allowing other tasks to make
// progress. Crash snapshots taken at these interleaving points exercise
// the recovery path under partial-apply conditions.

use crate::tests::support::YieldingCrashFile;

/// Concurrent writers with interleaved apply + crash + replay.
///
/// Two tasks write to different blocks concurrently while the apply task
/// yields between its writes (via `yield_on_write`). This creates a
/// genuine interleaving: one task's data may be at its final offset while
/// another task's WAL entry exists but hasn't been applied yet. After
/// crash + replay, all flushed data must be present.
#[async_test]
async fn concurrent_writes_interleaved_apply_crash(driver: DefaultDriver) {
    let (mem_file, _) = create_vhdx_with_block_size(8 * MB1, MB1 as u32).await;
    let snapshot = mem_file.snapshot();

    // yield_on_write=true: apply task yields before each page write,
    // allowing the log task to process another batch mid-apply.
    let file = YieldingCrashFile::from_durable(snapshot, true, false);
    let vhdx = Arc::new(VhdxFile::open(file).writable(&driver).await.unwrap());
    let block_size = vhdx.block_size();

    // Two concurrent writers to different blocks.
    {
        let vhdx_a = vhdx.clone();
        let vhdx_b = vhdx.clone();
        let bs = block_size;

        let ((), ()) = futures::join!(
            async {
                write_block(&*vhdx_a, 0, bs, 0xAA).await;
            },
            async {
                write_block(&*vhdx_b, bs as u64, bs, 0xBB).await;
            }
        );
    }

    // Flush to make everything durable.
    vhdx.flush().await.unwrap();

    // Take durable snapshot and crash.
    let durable = vhdx.file.durable_snapshot();
    Arc::into_inner(vhdx).expect("no other refs").abort().await;

    // Recover and verify both blocks survived.
    let recovered = InMemoryFile::from_snapshot(durable);
    let vhdx2 = VhdxFile::open(recovered)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();

    let buf0 = read_pattern_p16(&vhdx2, 0, block_size as usize).await;
    assert!(
        buf0.iter().all(|&b| b == 0xAA),
        "block 0 should have 0xAA after interleaved apply + crash"
    );

    let buf1 = read_pattern_p16(&vhdx2, block_size as u64, block_size as usize).await;
    assert!(
        buf1.iter().all(|&b| b == 0xBB),
        "block 1 should have 0xBB after interleaved apply + crash"
    );
}

/// Interleaved flush + write + crash.
///
/// `yield_on_flush=true` causes flush to yield, allowing a concurrent
/// writer to make progress (its write reaches the log task) before the
/// flush's file-level flush completes. After crash, the pre-flush data
/// must be durable; the concurrent write may or may not survive.
#[async_test]
async fn interleaved_flush_and_write_crash(driver: DefaultDriver) {
    let (mem_file, _) = create_vhdx_with_block_size(8 * MB1, MB1 as u32).await;
    let snapshot = mem_file.snapshot();

    // yield_on_flush=true: flush yields, allowing concurrent writer to run.
    let file = YieldingCrashFile::from_durable(snapshot, false, true);
    let vhdx = Arc::new(VhdxFile::open(file).writable(&driver).await.unwrap());
    let block_size = vhdx.block_size();

    // Write block 0 — this data must survive the flush.
    write_block(&*vhdx, 0, block_size, 0xCC).await;

    // Concurrent: flush (yields during file flush) + write block 1.
    {
        let vhdx_f = vhdx.clone();
        let vhdx_w = vhdx.clone();
        let bs = block_size;

        let ((), ()) = futures::join!(
            async {
                vhdx_f.flush().await.unwrap();
            },
            async {
                write_block(&*vhdx_w, bs as u64, bs, 0xDD).await;
            }
        );
    }

    // Final flush to ensure the concurrent write is also durable.
    vhdx.flush().await.unwrap();

    // Crash.
    let durable = vhdx.file.durable_snapshot();
    Arc::into_inner(vhdx).expect("no other refs").abort().await;

    // Recover.
    let recovered = InMemoryFile::from_snapshot(durable);
    let vhdx2 = VhdxFile::open(recovered)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();

    // Block 0 must survive (was written before the first flush).
    let buf0 = read_pattern_p16(&vhdx2, 0, block_size as usize).await;
    assert!(
        buf0.iter().all(|&b| b == 0xCC),
        "block 0 should have 0xCC (pre-flush data must survive)"
    );

    // Block 1 should also survive (final flush made it durable).
    let buf1 = read_pattern_p16(&vhdx2, block_size as u64, block_size as usize).await;
    assert!(
        buf1.iter().all(|&b| b == 0xDD),
        "block 1 should have 0xDD after final flush"
    );
}

/// Stress test: many interleaved writers with yielding apply + crash.
///
/// 8 tasks each write to a unique block with `yield_on_write=true`,
/// creating maximum interleaving between the apply task and log task.
/// After flush + crash + replay, all data must be intact.
#[async_test]
async fn stress_interleaved_apply_crash(driver: DefaultDriver) {
    let (mem_file, _) = create_vhdx_with_block_size(8 * MB1, MB1 as u32).await;
    let snapshot = mem_file.snapshot();

    let file = YieldingCrashFile::from_durable(snapshot, true, false);
    let vhdx = Arc::new(VhdxFile::open(file).writable(&driver).await.unwrap());
    let block_size = vhdx.block_size();

    // 8 concurrent writers, each to a unique block.
    let write_futures: Vec<_> = (0..8u8)
        .map(|i| {
            let vhdx = vhdx.clone();
            let bs = block_size;
            async move {
                let offset = i as u64 * bs as u64;
                let pattern = 0x50 + i;
                write_block(&*vhdx, offset, bs, pattern).await;
            }
        })
        .collect();

    futures::future::join_all(write_futures).await;

    vhdx.flush().await.unwrap();

    // Crash.
    let durable = vhdx.file.durable_snapshot();
    let vhdx = Arc::into_inner(vhdx).expect("no other refs");
    vhdx.abort().await;

    // Recover and verify all 8 blocks.
    let recovered = InMemoryFile::from_snapshot(durable);
    let vhdx2 = VhdxFile::open(recovered)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();

    for i in 0..8u8 {
        let offset = i as u64 * block_size as u64;
        let expected = 0x50 + i;
        let buf = read_pattern_p16(&vhdx2, offset, block_size as usize).await;
        assert!(
            buf.iter().all(|&b| b == expected),
            "block {i}: expected 0x{expected:02x}, got 0x{:02x}",
            buf[0]
        );
    }
}

/// Interleaved trim + write + crash with yield points.
///
/// Write all blocks, flush. Then concurrently trim some blocks and write
/// others with `yield_on_write=true`. Flush, crash, and verify the
/// expected state (trimmed blocks are zeros, written blocks have data).
#[async_test]
async fn interleaved_trim_write_crash(driver: DefaultDriver) {
    let (mem_file, _) = create_vhdx_with_block_size(8 * MB1, MB1 as u32).await;
    let snapshot = mem_file.snapshot();

    let file = YieldingCrashFile::from_durable(snapshot, true, false);
    let vhdx = Arc::new(VhdxFile::open(file).writable(&driver).await.unwrap());
    let block_size = vhdx.block_size();

    // Step 1: Write all 8 blocks with initial data.
    for i in 0..8u8 {
        let offset = i as u64 * block_size as u64;
        write_block(&*vhdx, offset, block_size, 0x10 + i).await;
    }
    vhdx.flush().await.unwrap();

    // Step 2: Concurrently trim blocks 0-3 and write blocks 4-7.
    let trim_futures: Vec<_> = (0..4u8)
        .map(|i| {
            let vhdx = vhdx.clone();
            let bs = block_size;
            async move {
                let offset = i as u64 * bs as u64;
                vhdx.trim(TrimRequest::new(TrimMode::FileSpace, offset, bs as u64))
                    .await
                    .unwrap();
            }
        })
        .collect();

    let write_futures: Vec<_> = (4..8u8)
        .map(|i| {
            let vhdx = vhdx.clone();
            let bs = block_size;
            async move {
                let offset = i as u64 * bs as u64;
                write_block(&*vhdx, offset, bs, 0x90 + i).await;
            }
        })
        .collect();

    let ((), ()) = futures::join!(
        async {
            futures::future::join_all(trim_futures).await;
        },
        async {
            futures::future::join_all(write_futures).await;
        }
    );

    vhdx.flush().await.unwrap();

    // Crash.
    let durable = vhdx.file.durable_snapshot();
    let vhdx = Arc::into_inner(vhdx).expect("no other refs");
    vhdx.abort().await;

    // Recover.
    let recovered = InMemoryFile::from_snapshot(durable);
    let vhdx2 = VhdxFile::open(recovered)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();

    // Blocks 0-3: trimmed → zeros.
    for i in 0..4u8 {
        let offset = i as u64 * block_size as u64;
        let buf = read_pattern_p16(&vhdx2, offset, block_size as usize).await;
        assert!(
            buf.iter().all(|&b| b == 0),
            "block {i}: expected zeros (trimmed), got 0x{:02x}",
            buf[0]
        );
    }

    // Blocks 4-7: overwritten with new data.
    for i in 4..8u8 {
        let offset = i as u64 * block_size as u64;
        let expected = 0x90 + i;
        let buf = read_pattern_p16(&vhdx2, offset, block_size as usize).await;
        assert!(
            buf.iter().all(|&b| b == expected),
            "block {i}: expected 0x{expected:02x}, got 0x{:02x}",
            buf[0]
        );
    }
}

// ---- Selective durability crash tests ----
//
// These tests use CrashAfterFlushFile to crash at specific points in
// the WAL pipeline. Unlike CrashTestFile (where flush is all-or-nothing),
// CrashAfterFlushFile can be armed to fail after N more flushes,
// simulating crashes between the WAL flush and the apply flush.

use crate::tests::support::CrashAfterFlushFile;

/// Write + flush with crash armed after 1 flush.
///
/// The VhdxFile::flush() path does: commit → log task writes WAL →
/// flush_sequencer.flush() (1 file.flush()) → apply task writes BAT.
///
/// With arm(1), the flush_sequencer's flush succeeds (WAL + user data
/// durable), but a subsequent flush (or the apply write itself) fails.
/// The apply task's BAT write may or may not succeed in volatile, but
/// the BAT is NOT durable. On recovery, WAL replay must restore the
/// BAT page.
#[async_test]
async fn crash_wal_durable_apply_lost(driver: DefaultDriver) {
    let (mem_file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let snapshot = mem_file.snapshot();

    let crash_file = CrashAfterFlushFile::new(snapshot);
    let vhdx = VhdxFile::open(crash_file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size() as usize;

    // Write one block.
    write_pattern_p16(&vhdx, 0, block_size, 0xAB).await;

    // Arm the crash: allow 1 more flush (the flush_sequencer's flush
    // that makes WAL + user data durable), then fail everything.
    vhdx.file.arm(1);

    // Flush — the WAL flush succeeds; subsequent ops fail.
    // This may return Ok (if the crash hits after the sequencer flush)
    // or Err (if the apply task races and triggers the error).
    let _ = vhdx.flush().await;

    // Take the durable snapshot. The WAL entry and user data should be
    // durable. The BAT page may NOT be at its final offset.
    let durable = vhdx.file.durable_snapshot();

    // Don't call abort() — the file is poisoned, tasks may be in error state.
    // Just drop everything and recover from durable state.
    drop(vhdx);

    // Recover: open with replay. The WAL should restore the BAT page.
    let recovered = InMemoryFile::from_snapshot(durable);
    let vhdx2 = VhdxFile::open(recovered)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();

    // Verify the data survived via WAL replay.
    let buf = read_pattern_p16(&vhdx2, 0, block_size).await;
    assert!(
        buf.iter().all(|&b| b == 0xAB),
        "data should survive via WAL replay when apply is lost: got 0x{:02x}",
        buf[0]
    );
}

/// Write + flush with crash armed after 0 flushes.
///
/// The next flush fails immediately. Nothing new is durable.
/// Recovery should see the original empty state.
#[async_test]
async fn crash_before_wal_flush_data_lost(driver: DefaultDriver) {
    let (mem_file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let snapshot = mem_file.snapshot();

    let crash_file = CrashAfterFlushFile::new(snapshot.clone());
    let vhdx = VhdxFile::open(crash_file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size() as usize;

    // Write a block.
    write_pattern_p16(&vhdx, 0, block_size, 0xCD).await;

    // Arm: next flush fails. The WAL flush won't succeed, so nothing
    // new is durable.
    vhdx.file.arm(0);

    // Flush will fail.
    let result = vhdx.flush().await;
    assert!(result.is_err(), "flush should fail with armed crash");

    // Durable state should be the pre-write state.
    let durable = vhdx.file.durable_snapshot();
    drop(vhdx);

    // Recover and verify data is NOT present (was never durable).
    let recovered = InMemoryFile::from_snapshot(durable);
    let vhdx2 = VhdxFile::open(recovered)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();

    let buf = read_pattern_p16(&vhdx2, 0, block_size).await;
    assert!(
        buf.iter().all(|&b| b == 0),
        "data should be lost when WAL flush fails: got 0x{:02x}",
        buf[0]
    );
}

/// Multiple writes, flush, then arm and write more.
///
/// First batch: write blocks 0-2, flush (all durable). Second batch:
/// write blocks 3-4, arm(1), flush (WAL durable, apply may fail).
/// Recovery should see blocks 0-2 (clean) and blocks 3-4 (via replay).
#[async_test]
async fn crash_partial_pipeline_multi_batch(driver: DefaultDriver) {
    let (mem_file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let snapshot = mem_file.snapshot();

    let crash_file = CrashAfterFlushFile::new(snapshot);
    let vhdx = VhdxFile::open(crash_file).writable(&driver).await.unwrap();
    let block_size = vhdx.block_size() as u64;

    // Batch 1: write blocks 0-2, flush normally (unarmed).
    for i in 0..3u8 {
        let offset = i as u64 * block_size;
        write_pattern_p16(&vhdx, offset, block_size as usize, 0x10 + i).await;
    }
    vhdx.flush().await.unwrap();

    // Batch 2: write blocks 3-4, arm, flush.
    for i in 3..5u8 {
        let offset = i as u64 * block_size;
        write_pattern_p16(&vhdx, offset, block_size as usize, 0x20 + i).await;
    }

    // Arm: 1 more flush (WAL flush succeeds), then crash.
    vhdx.file.arm(1);
    let _ = vhdx.flush().await;

    let durable = vhdx.file.durable_snapshot();
    drop(vhdx);

    // Recover.
    let recovered = InMemoryFile::from_snapshot(durable);
    let vhdx2 = VhdxFile::open(recovered)
        .allow_replay(true)
        .read_only()
        .await
        .unwrap();

    // Blocks 0-2: from batch 1 (fully durable before arm).
    for i in 0..3u8 {
        let offset = i as u64 * block_size;
        let expected = 0x10 + i;
        let buf = read_pattern_p16(&vhdx2, offset, block_size as usize).await;
        assert!(
            buf.iter().all(|&b| b == expected),
            "batch 1 block {i}: expected 0x{expected:02x}, got 0x{:02x}",
            buf[0]
        );
    }

    // Blocks 3-4: from batch 2 (WAL durable, may need replay).
    for i in 3..5u8 {
        let offset = i as u64 * block_size;
        let expected = 0x20 + i;
        let buf = read_pattern_p16(&vhdx2, offset, block_size as usize).await;
        assert!(
            buf.iter().all(|&b| b == expected),
            "batch 2 block {i}: expected 0x{expected:02x}, got 0x{:02x}",
            buf[0]
        );
    }
}
