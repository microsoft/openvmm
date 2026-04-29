// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Sector bitmap primitives for partially-present VHDX blocks.
//!
//! A sector bitmap is a 1-MiB block of bits where each bit represents one
//! logical sector in the chunk. Bits are stored LSB-first within each byte.
//! Higher-level guest-visible I/O consumes these helpers in a later chunk.

#![allow(dead_code)]

use crate::format::CACHE_PAGE_SIZE;
use crate::format::SECTORS_PER_CHUNK;
use bitvec::prelude::*;
use std::ops::Range;

/// Cache tag for sector bitmap pages.
///
/// SBM pages are at absolute file offsets, so the tag base is registered as 0.
pub(crate) const SBM_TAG: u8 = 2;

/// Number of sectors tracked per bitmap cache page (4 KiB * 8 bits = 32768).
pub(crate) const SECTORS_PER_BITMAP_PAGE: u64 = CACHE_PAGE_SIZE * 8;

/// A contiguous sector-bitmap range contained within a single cache page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SectorBitmapPageRange {
    /// Chunk containing the virtual sectors.
    pub chunk_number: u32,
    /// Sector-bitmap page number within the 1-MiB bitmap block.
    pub page_number: u64,
    /// First bit in the bitmap page.
    pub start_bit: u64,
    /// Number of bits/sectors covered by this range.
    pub bit_count: u64,
    /// Virtual disk offset corresponding to `start_bit`.
    pub virtual_offset: u64,
}

impl SectorBitmapPageRange {
    /// Absolute file offset of this bitmap page.
    pub fn page_file_offset(self, sbm_block_file_offset: u64) -> Option<u64> {
        sbm_block_file_offset.checked_add(self.page_number.checked_mul(CACHE_PAGE_SIZE)?)
    }
}

/// Whether a run of sectors is present in this VHDX file or transparent to a
/// parent layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SectorBitmapRunKind {
    Present,
    Transparent,
}

/// A contiguous run decoded from sector bitmap bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SectorBitmapRun {
    pub kind: SectorBitmapRunKind,
    pub virtual_offset: u64,
    pub block_offset: u32,
    pub length: u32,
}

/// Input validation error for sector-bitmap primitives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SectorBitmapError {
    InvalidSectorSize,
    UnalignedRange,
    RangeOverflow,
    BitmapRangeOutOfBounds,
}

/// Split a virtual byte range into per-bitmap-page ranges.
pub(crate) fn bitmap_page_ranges(
    logical_sector_size: u32,
    virtual_offset: u64,
    length: u32,
) -> Result<Vec<SectorBitmapPageRange>, SectorBitmapError> {
    if logical_sector_size == 0 {
        return Err(SectorBitmapError::InvalidSectorSize);
    }

    let logical_sector_size = logical_sector_size as u64;
    if !virtual_offset.is_multiple_of(logical_sector_size)
        || u64::from(length) % logical_sector_size != 0
    {
        return Err(SectorBitmapError::UnalignedRange);
    }

    virtual_offset
        .checked_add(u64::from(length))
        .ok_or(SectorBitmapError::RangeOverflow)?;

    let mut ranges = Vec::new();
    let mut remaining_sectors = u64::from(length) / logical_sector_size;
    let mut current_virtual_offset = virtual_offset;

    while remaining_sectors > 0 {
        let cur_sector = current_virtual_offset / logical_sector_size;
        let chunk_number = (cur_sector / SECTORS_PER_CHUNK) as u32;
        let cur_chunk_sector = cur_sector % SECTORS_PER_CHUNK;
        let page_number = cur_chunk_sector / SECTORS_PER_BITMAP_PAGE;
        let start_bit = cur_chunk_sector % SECTORS_PER_BITMAP_PAGE;
        let end_bit = (start_bit + remaining_sectors).min(SECTORS_PER_BITMAP_PAGE);
        let bit_count = end_bit - start_bit;

        ranges.push(SectorBitmapPageRange {
            chunk_number,
            page_number,
            start_bit,
            bit_count,
            virtual_offset: current_virtual_offset,
        });

        let bytes_processed = bit_count
            .checked_mul(logical_sector_size)
            .ok_or(SectorBitmapError::RangeOverflow)?;
        current_virtual_offset = current_virtual_offset
            .checked_add(bytes_processed)
            .ok_or(SectorBitmapError::RangeOverflow)?;
        remaining_sectors -= bit_count;
    }

    Ok(ranges)
}

/// Decode present/transparent runs from a single sector-bitmap page window.
pub(crate) fn bitmap_runs_in_page(
    bitmap_page: &[u8],
    logical_sector_size: u32,
    block_size: u32,
    virtual_offset: u64,
    start_bit: u64,
    bit_count: u64,
) -> Result<Vec<SectorBitmapRun>, SectorBitmapError> {
    if logical_sector_size == 0 || block_size == 0 {
        return Err(SectorBitmapError::InvalidSectorSize);
    }

    let logical_sector_size = logical_sector_size as u64;
    let window_range = checked_bit_range(bitmap_page, start_bit, bit_count)?;
    let bits = BitSlice::<u8, Lsb0>::from_slice(bitmap_page);
    let window = &bits[window_range];

    let mut runs = Vec::new();
    let mut pos = 0usize;
    let mut current_virtual_offset = virtual_offset;

    while pos < window.len() {
        let present = window[pos];
        let end = if present {
            window[pos..].first_zero().map_or(window.len(), |i| pos + i)
        } else {
            window[pos..].first_one().map_or(window.len(), |i| pos + i)
        };
        let sector_count = (end - pos) as u64;
        let byte_count = sector_count
            .checked_mul(logical_sector_size)
            .ok_or(SectorBitmapError::RangeOverflow)?;
        let length = u32::try_from(byte_count).map_err(|_| SectorBitmapError::RangeOverflow)?;

        runs.push(SectorBitmapRun {
            kind: if present {
                SectorBitmapRunKind::Present
            } else {
                SectorBitmapRunKind::Transparent
            },
            virtual_offset: current_virtual_offset,
            block_offset: (current_virtual_offset % u64::from(block_size)) as u32,
            length,
        });

        current_virtual_offset = current_virtual_offset
            .checked_add(byte_count)
            .ok_or(SectorBitmapError::RangeOverflow)?;
        pos = end;
    }

    Ok(runs)
}

/// Set or clear a bit range in a sector-bitmap page.
///
/// Returns `true` if any bit changed.
pub(crate) fn set_bitmap_bits(
    bitmap_page: &mut [u8],
    start_bit: u64,
    bit_count: u64,
    set: bool,
) -> Result<bool, SectorBitmapError> {
    let window_range = checked_bit_range(bitmap_page, start_bit, bit_count)?;
    if window_range.is_empty() {
        return Ok(false);
    }

    let bits = BitSlice::<u8, Lsb0>::from_slice(bitmap_page);
    let window = &bits[window_range.clone()];
    let needs_change = if set { !window.all() } else { window.any() };

    if needs_change {
        let bits = BitSlice::<u8, Lsb0>::from_slice_mut(bitmap_page);
        bits[window_range].fill(set);
    }

    Ok(needs_change)
}

fn checked_bit_range(
    bitmap_page: &[u8],
    start_bit: u64,
    bit_count: u64,
) -> Result<Range<usize>, SectorBitmapError> {
    let end_bit = start_bit
        .checked_add(bit_count)
        .ok_or(SectorBitmapError::RangeOverflow)?;
    let bit_len = bitmap_page
        .len()
        .checked_mul(8)
        .ok_or(SectorBitmapError::RangeOverflow)?;
    if end_bit > bit_len as u64 {
        return Err(SectorBitmapError::BitmapRangeOutOfBounds);
    }
    Ok(start_bit as usize..end_bit as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECTOR: u32 = 512;
    const BLOCK: u32 = 2 * 1024 * 1024;

    #[test]
    fn bitmap_page_ranges_split_at_page_boundary() {
        let start_sector = SECTORS_PER_BITMAP_PAGE - 2;
        let ranges = bitmap_page_ranges(SECTOR, start_sector * u64::from(SECTOR), 4 * SECTOR)
            .expect("range is valid");

        assert_eq!(
            ranges,
            vec![
                SectorBitmapPageRange {
                    chunk_number: 0,
                    page_number: 0,
                    start_bit: SECTORS_PER_BITMAP_PAGE - 2,
                    bit_count: 2,
                    virtual_offset: start_sector * u64::from(SECTOR),
                },
                SectorBitmapPageRange {
                    chunk_number: 0,
                    page_number: 1,
                    start_bit: 0,
                    bit_count: 2,
                    virtual_offset: SECTORS_PER_BITMAP_PAGE * u64::from(SECTOR),
                },
            ]
        );
    }

    #[test]
    fn bitmap_page_ranges_split_at_chunk_boundary() {
        let start_sector = SECTORS_PER_CHUNK - 1;
        let ranges = bitmap_page_ranges(SECTOR, start_sector * u64::from(SECTOR), 2 * SECTOR)
            .expect("range is valid");

        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].chunk_number, 0);
        assert_eq!(
            ranges[0].page_number,
            (SECTORS_PER_CHUNK - 1) / SECTORS_PER_BITMAP_PAGE
        );
        assert_eq!(ranges[0].start_bit, SECTORS_PER_BITMAP_PAGE - 1);
        assert_eq!(ranges[0].bit_count, 1);
        assert_eq!(ranges[1].chunk_number, 1);
        assert_eq!(ranges[1].page_number, 0);
        assert_eq!(ranges[1].start_bit, 0);
        assert_eq!(ranges[1].bit_count, 1);
    }

    #[test]
    fn bitmap_runs_use_lsb_first_ordering() {
        let bitmap = [0b0101_0011];
        let runs = bitmap_runs_in_page(&bitmap, SECTOR, BLOCK, 0, 0, 8).expect("valid bitmap");

        assert_eq!(
            runs,
            vec![
                SectorBitmapRun {
                    kind: SectorBitmapRunKind::Present,
                    virtual_offset: 0,
                    block_offset: 0,
                    length: 2 * SECTOR,
                },
                SectorBitmapRun {
                    kind: SectorBitmapRunKind::Transparent,
                    virtual_offset: 2 * u64::from(SECTOR),
                    block_offset: 2 * SECTOR,
                    length: 2 * SECTOR,
                },
                SectorBitmapRun {
                    kind: SectorBitmapRunKind::Present,
                    virtual_offset: 4 * u64::from(SECTOR),
                    block_offset: 4 * SECTOR,
                    length: SECTOR,
                },
                SectorBitmapRun {
                    kind: SectorBitmapRunKind::Transparent,
                    virtual_offset: 5 * u64::from(SECTOR),
                    block_offset: 5 * SECTOR,
                    length: SECTOR,
                },
                SectorBitmapRun {
                    kind: SectorBitmapRunKind::Present,
                    virtual_offset: 6 * u64::from(SECTOR),
                    block_offset: 6 * SECTOR,
                    length: SECTOR,
                },
                SectorBitmapRun {
                    kind: SectorBitmapRunKind::Transparent,
                    virtual_offset: 7 * u64::from(SECTOR),
                    block_offset: 7 * SECTOR,
                    length: SECTOR,
                },
            ]
        );
    }

    #[test]
    fn bitmap_runs_preserve_block_offsets() {
        let bitmap = [0xff];
        let virtual_offset = u64::from(BLOCK) + 4 * u64::from(SECTOR);
        let runs = bitmap_runs_in_page(&bitmap, SECTOR, BLOCK, virtual_offset, 0, 4)
            .expect("valid bitmap");

        assert_eq!(
            runs,
            vec![SectorBitmapRun {
                kind: SectorBitmapRunKind::Present,
                virtual_offset,
                block_offset: 4 * SECTOR,
                length: 4 * SECTOR,
            }]
        );
    }

    #[test]
    fn set_bitmap_bits_sets_and_clears_lsb_first_ranges() {
        let mut bitmap = [0u8; 2];

        assert!(set_bitmap_bits(&mut bitmap, 0, 1, true).expect("valid bitmap"));
        assert!(set_bitmap_bits(&mut bitmap, 7, 2, true).expect("valid bitmap"));
        assert_eq!(bitmap, [0b1000_0001, 0b0000_0001]);

        assert!(!set_bitmap_bits(&mut bitmap, 0, 1, true).expect("valid bitmap"));
        assert!(set_bitmap_bits(&mut bitmap, 7, 1, false).expect("valid bitmap"));
        assert_eq!(bitmap, [0b0000_0001, 0b0000_0001]);
    }

    #[test]
    fn set_bitmap_bits_can_fill_whole_page() {
        let mut bitmap = [0u8; CACHE_PAGE_SIZE as usize];
        assert!(
            set_bitmap_bits(&mut bitmap, 0, SECTORS_PER_BITMAP_PAGE, true).expect("valid bitmap")
        );
        assert!(bitmap.iter().all(|byte| *byte == 0xff));

        assert!(
            set_bitmap_bits(&mut bitmap, 0, SECTORS_PER_BITMAP_PAGE, false).expect("valid bitmap")
        );
        assert!(bitmap.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn helpers_reject_invalid_ranges() {
        assert_eq!(
            bitmap_page_ranges(0, 0, SECTOR),
            Err(SectorBitmapError::InvalidSectorSize)
        );
        assert_eq!(
            bitmap_page_ranges(SECTOR, 1, SECTOR),
            Err(SectorBitmapError::UnalignedRange)
        );
        assert_eq!(
            set_bitmap_bits(&mut [0u8; 1], 8, 1, true),
            Err(SectorBitmapError::BitmapRangeOutOfBounds)
        );
    }
}
