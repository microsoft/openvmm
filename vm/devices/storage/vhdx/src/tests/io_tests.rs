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
