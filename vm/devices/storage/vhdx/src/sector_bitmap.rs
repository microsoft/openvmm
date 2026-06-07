// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Sector bitmap read logic for partially-present VHDX blocks.
//!
//! A sector bitmap is a 1-MiB block of bits where each bit represents one
//! logical sector. Bit = 1 means the sector's data is present in this VHDX
//! file; bit = 0 means the sector is transparent (should be read from the
//! parent disk in a differencing chain).
//!
//! The bitmap is cached in 4-KiB pages via the [`PageCache`]. Each page
//! covers `4096 * 8 = 32768` sectors.

use crate::AsyncFile;
use crate::cache::PageKey;
use crate::cache::WriteMode;
use crate::error::VhdxIoError;
use crate::error::VhdxIoErrorInner;
use crate::format::BatEntryState;
use crate::format::CACHE_PAGE_SIZE;
use crate::format::SECTORS_PER_CHUNK;
use crate::io::ReadRange;
use crate::open::VhdxFile;
use bitvec::prelude::*;

/// Cache tag for sector bitmap pages.
///
/// SBM pages are at absolute file offsets (not region-relative), so the
/// base offset for this tag is 0.
pub(crate) const SBM_TAG: u8 = 2;

/// Number of sectors tracked per bitmap cache page (4 KiB * 8 bits = 32768).
const SECTORS_PER_BITMAP_PAGE: u64 = CACHE_PAGE_SIZE * 8;

impl<F: AsyncFile> VhdxFile<F> {
    /// Resolve a read for a partially-present block by reading the sector bitmap.
    ///
    /// For each sector in the range, checks the corresponding bit in the sector
    /// bitmap. Emits runs of [`ReadRange::Data`] (bit=1, sector present in file)
    /// and [`ReadRange::Unmapped`] (bit=0, sector transparent to parent).
    ///
    /// # Arguments
    ///
    /// * `data_file_offset` - The file offset of the data block (from the
    ///   payload BAT entry). Used to compute file offsets for present sectors.
    /// * `virtual_offset` - The virtual disk byte offset of the start of this
    ///   sub-request (already clamped to a single block).
    /// * `length` - The length in bytes (already clamped to a single block).
    /// * `ranges` - Output vector to append ranges to.
    pub(crate) async fn resolve_partial_block_read(
        &self,
        data_file_offset: u64,
        virtual_offset: u64,
        length: u32,
        ranges: &mut Vec<ReadRange>,
    ) -> Result<(), VhdxIoError> {
        // 1. Compute sector coordinates.
        let sector_number = virtual_offset / self.logical_sector_size as u64;
        let chunk_number = (sector_number / SECTORS_PER_CHUNK) as u32;
        let sector_count = length as u64 / self.logical_sector_size as u64;

        // 2. Get sector bitmap block mapping (synchronous).
        // The SBM is guaranteed to be allocated — validated during BAT
        // loading (PartiallyPresentWithoutSectorBitmap check) and
        // maintained at runtime by ensure_sbm_allocated.
        let sbm_mapping = self.bat.get_sector_bitmap_mapping(chunk_number);
        assert!(
            sbm_mapping.bat_state() == BatEntryState::FullyPresent,
            "SBM for chunk {chunk_number} must be allocated for PartiallyPresent block"
        );

        // 3. Iterate over bitmap pages (outer loop for multi-page support).
        let mut remaining_sectors = sector_count;
        let mut current_virtual_offset = virtual_offset;

        while remaining_sectors > 0 {
            // Recompute bitmap page coordinates for current position.
            let cur_sector = current_virtual_offset / self.logical_sector_size as u64;
            let cur_chunk_sector = cur_sector % SECTORS_PER_CHUNK;
            let cur_page_number = cur_chunk_sector / SECTORS_PER_BITMAP_PAGE;
            let start_bit = cur_chunk_sector % SECTORS_PER_BITMAP_PAGE;
            let bits_in_this_page =
                std::cmp::min(start_bit + remaining_sectors, SECTORS_PER_BITMAP_PAGE);

            // Acquire the bitmap page for this portion.
            let page_file_offset = sbm_mapping.file_offset() + cur_page_number * CACHE_PAGE_SIZE;
            {
                let guard = self
                    .cache
                    .acquire_read(PageKey {
                        tag: SBM_TAG,
                        offset: page_file_offset,
                    })
                    .await
                    .map_err(VhdxIoErrorInner::ReadSectorBitmap)?;

                // Scan bits within this page using BitSlice for word-level acceleration.
                let bits = BitSlice::<u8, Lsb0>::from_slice(&*guard);
                let window = &bits[start_bit as usize..bits_in_this_page as usize];
                let mut pos = 0usize;
                let len = window.len();
                while pos < len {
                    // Find first set bit (data present).
                    let one = window[pos..].first_one().map_or(len, |i| pos + i);
                    if one > pos {
                        let unmapped_sectors = (one - pos) as u64;
                        let unmapped_bytes = unmapped_sectors * self.logical_sector_size as u64;
                        ranges.push(ReadRange::Unmapped {
                            guest_offset: current_virtual_offset,
                            length: unmapped_bytes as u32,
                        });
                        current_virtual_offset += unmapped_bytes;
                    }

                    if one < len {
                        // Find first clear bit (end of data run).
                        let next_zero = window[one..].first_zero().map_or(len, |i| one + i);
                        let data_sectors = (next_zero - one) as u64;
                        let data_bytes = data_sectors * self.logical_sector_size as u64;
                        let block_offset = (current_virtual_offset % self.block_size as u64) as u32;
                        let file_offset = data_file_offset + block_offset as u64;
                        ranges.push(ReadRange::Data {
                            guest_offset: current_virtual_offset,
                            length: data_bytes as u32,
                            file_offset,
                        });
                        current_virtual_offset += data_bytes;
                        pos = next_zero;
                    } else {
                        pos = len;
                    }
                }
            }

            // Advance to next page.
            let sectors_processed = bits_in_this_page - start_bit;
            remaining_sectors -= sectors_processed;
        }

        Ok(())
    }

    /// Set or clear sector bitmap bits for a range of sectors.
    ///
    /// For each sector in the virtual range, sets (or clears) the corresponding
    /// bit in the sector bitmap. The bitmap page is acquired in Modify mode
    /// and written through to disk on release.
    ///
    /// # Arguments
    ///
    /// * `virtual_offset` - Virtual disk byte offset of the start of the range.
    /// * `length` - Length in bytes.
    /// * `set` - If true, set bits (mark sectors present); if false, clear bits.
    pub(crate) async fn set_sector_bitmap_bits(
        &self,
        virtual_offset: u64,
        length: u32,
        set: bool,
    ) -> Result<(), VhdxIoError> {
        let sector_number = virtual_offset / self.logical_sector_size as u64;
        let chunk_number = (sector_number / SECTORS_PER_CHUNK) as u32;
        let sector_count = length as u64 / self.logical_sector_size as u64;

        // Get sector bitmap block mapping (synchronous).
        // The SBM is guaranteed to be allocated — validated during BAT
        // loading and maintained at runtime by ensure_sbm_allocated.
        let sbm_mapping = self.bat.get_sector_bitmap_mapping(chunk_number);
        assert!(
            sbm_mapping.bat_state() == BatEntryState::FullyPresent,
            "SBM for chunk {chunk_number} must be allocated for PartiallyPresent block"
        );

        let mut remaining_sectors = sector_count;
        let mut current_virtual_offset = virtual_offset;

        while remaining_sectors > 0 {
            let cur_sector = current_virtual_offset / self.logical_sector_size as u64;
            let cur_chunk_sector = cur_sector % SECTORS_PER_CHUNK;
            let cur_page_number = cur_chunk_sector / SECTORS_PER_BITMAP_PAGE;
            let start_bit = cur_chunk_sector % SECTORS_PER_BITMAP_PAGE;
            let bits_in_this_page =
                std::cmp::min(start_bit + remaining_sectors, SECTORS_PER_BITMAP_PAGE);

            let page_file_offset = sbm_mapping.file_offset() + cur_page_number * CACHE_PAGE_SIZE;

            // If the range covers the entire page, skip the disk read.
            let full_page = start_bit == 0 && bits_in_this_page == SECTORS_PER_BITMAP_PAGE;
            let mode = if full_page {
                WriteMode::Overwrite
            } else {
                WriteMode::Modify
            };

            let mut guard = self
                .cache
                .acquire_write(
                    PageKey {
                        tag: SBM_TAG,
                        offset: page_file_offset,
                    },
                    mode,
                )
                .await
                .map_err(VhdxIoErrorInner::SectorBitmapCache)?;

            if full_page {
                // Overwrite entire page without reading existing data.
                // Overwriting pages are zero-initialized by the cache.
                if set || !guard.is_overwriting() {
                    guard.fill(if set { 0xFF } else { 0x00 });
                }
            } else {
                // Check via read-only Deref whether any bits actually differ.
                // If not, DerefMut is never called, the page stays clean,
                // and no write-back occurs.
                let bits = BitSlice::<u8, Lsb0>::from_slice(&*guard);
                let window = &bits[start_bit as usize..bits_in_this_page as usize];
                let needs_change = if set { !window.all() } else { window.any() };

                if needs_change {
                    let bits_mut = BitSlice::<u8, Lsb0>::from_slice_mut(&mut *guard);
                    bits_mut[start_bit as usize..bits_in_this_page as usize].fill(set);
                }
            }

            let sectors_processed = bits_in_this_page - start_bit;
            remaining_sectors -= sectors_processed;
            current_virtual_offset += sectors_processed * self.logical_sector_size as u64;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AsyncFileExt;
    use crate::bat::Bat;
    use crate::create::{self, CreateParams};
    use crate::format;
    use crate::format::BatEntry;
    use crate::io::ReadRange;
    use crate::open::VhdxFile;
    use crate::region;
    use crate::tests::support::InMemoryFile;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use zerocopy::IntoBytes;

    /// Create a differencing VHDX with block 0 set to PartiallyPresent and
    /// a sector bitmap at a known file offset.
    ///
    /// The `bitmap_data` should be exactly 4096 bytes of bitmap data for the
    /// first bitmap page.
    ///
    /// Returns `(VhdxFile, data_block_file_offset, sbm_block_file_offset)`.
    async fn create_partial_block_vhdx(
        bitmap_data: &[u8; 4096],
    ) -> (VhdxFile<InMemoryFile>, u64, u64) {
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size: format::GB1,
            has_parent: true,
            ..Default::default()
        };
        create::create(&file, &mut params).await.unwrap();

        // Parse regions to find the BAT offset.
        let regions = region::parse_region_tables(&file).await.unwrap();
        let bat_offset = regions.bat_offset;

        // Compute entry indices. With 2 MiB blocks, 512-byte sectors,
        // chunk_ratio = 2048. Block 0 is payload entry 0. SBM entry for
        // chunk 0 is at index chunk_ratio = 2048.
        let bat = Bat::new(
            format::GB1,
            format::DEFAULT_BLOCK_SIZE,
            512,
            true,
            format::MB1 as u32,
        )
        .unwrap();
        let payload_index = bat.payload_entry_index(0);
        let sbm_index = bat.sector_bitmap_entry_index(0);

        // Place data block at 8 MiB (file_offset_mb = 8).
        let data_block_offset = 8 * format::MB1;
        let data_entry = BatEntry::new()
            .with_state(BatEntryState::PartiallyPresent as u8)
            .with_file_offset_mb(data_block_offset >> 20);
        file.write_at(bat_offset + payload_index as u64 * 8, data_entry.as_bytes())
            .await
            .unwrap();

        // Place SBM block at 10 MiB (file_offset_mb = 10).
        let sbm_block_offset = 10 * format::MB1;
        let sbm_entry = BatEntry::new()
            .with_state(BatEntryState::FullyPresent as u8)
            .with_file_offset_mb(sbm_block_offset >> 20);
        file.write_at(bat_offset + sbm_index as u64 * 8, sbm_entry.as_bytes())
            .await
            .unwrap();

        // Write the bitmap data at the SBM page offset (first page of SBM block).
        file.write_at(sbm_block_offset, bitmap_data).await.unwrap();

        // Open the VHDX.
        let vhdx = VhdxFile::open(file).read_only().await.unwrap();

        (vhdx, data_block_offset, sbm_block_offset)
    }

    #[async_test]
    async fn partial_block_all_present() {
        // All bits set → single Data range.
        let bitmap = [0xFFu8; 4096];
        let (vhdx, data_offset, _) = create_partial_block_vhdx(&bitmap).await;

        let mut ranges = Vec::new();
        // Read first 4096 bytes (8 sectors * 512) of block 0.
        vhdx.resolve_read(0, 4096, &mut ranges).await.unwrap();

        assert_eq!(ranges.len(), 1);
        assert_eq!(
            ranges[0],
            ReadRange::Data {
                guest_offset: 0,
                length: 4096,
                file_offset: data_offset,
            }
        );
    }

    #[async_test]
    async fn partial_block_all_transparent() {
        // All bits clear → single Unmapped range.
        let bitmap = [0x00u8; 4096];
        let (vhdx, _, _) = create_partial_block_vhdx(&bitmap).await;

        let mut ranges = Vec::new();
        vhdx.resolve_read(0, 4096, &mut ranges).await.unwrap();

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
    async fn partial_block_mixed() {
        // First 4 sectors (bits 0-3) set, next 4 (bits 4-7) clear.
        // Byte 0 = 0x0F (bits 0-3 set, 4-7 clear).
        let mut bitmap = [0x00u8; 4096];
        bitmap[0] = 0x0F;
        let (vhdx, data_offset, _) = create_partial_block_vhdx(&bitmap).await;

        let mut ranges = Vec::new();
        // Read 8 sectors = 4096 bytes.
        vhdx.resolve_read(0, 4096, &mut ranges).await.unwrap();

        assert_eq!(ranges.len(), 2);
        assert_eq!(
            ranges[0],
            ReadRange::Data {
                guest_offset: 0,
                length: 2048, // 4 sectors * 512
                file_offset: data_offset,
            }
        );
        assert_eq!(
            ranges[1],
            ReadRange::Unmapped {
                guest_offset: 2048,
                length: 2048,
            }
        );
    }

    #[async_test]
    async fn partial_block_alternating() {
        // Alternating: sector 0 set, 1 clear, 2 set, 3 clear, ...
        // Byte pattern: 0b01010101 = 0x55 → bits 0,2,4,6 set
        let mut bitmap = [0x00u8; 4096];
        bitmap[0] = 0x55;
        let (vhdx, data_offset, _) = create_partial_block_vhdx(&bitmap).await;

        let mut ranges = Vec::new();
        // Read 8 sectors = 4096 bytes.
        vhdx.resolve_read(0, 4096, &mut ranges).await.unwrap();

        // 0x55 = 0b01010101: bits 0,2,4,6 set; bits 1,3,5,7 clear.
        // Expected: Data(0) Unmapped(1) Data(2) Unmapped(3)
        //           Data(4) Unmapped(5) Data(6) Unmapped(7)
        assert_eq!(ranges.len(), 8);
        assert_eq!(
            ranges[0],
            ReadRange::Data {
                guest_offset: 0,
                length: 512,
                file_offset: data_offset,
            }
        );
        assert_eq!(
            ranges[1],
            ReadRange::Unmapped {
                guest_offset: 512,
                length: 512,
            }
        );
        assert_eq!(
            ranges[2],
            ReadRange::Data {
                guest_offset: 1024,
                length: 512,
                file_offset: data_offset + 1024,
            }
        );
        assert_eq!(
            ranges[3],
            ReadRange::Unmapped {
                guest_offset: 1536,
                length: 512,
            }
        );
        assert_eq!(
            ranges[4],
            ReadRange::Data {
                guest_offset: 2048,
                length: 512,
                file_offset: data_offset + 2048,
            }
        );
        assert_eq!(
            ranges[5],
            ReadRange::Unmapped {
                guest_offset: 2560,
                length: 512,
            }
        );
        assert_eq!(
            ranges[6],
            ReadRange::Data {
                guest_offset: 3072,
                length: 512,
                file_offset: data_offset + 3072,
            }
        );
        assert_eq!(
            ranges[7],
            ReadRange::Unmapped {
                guest_offset: 3584,
                length: 512,
            }
        );
    }

    #[async_test]
    async fn partial_block_single_sector_present() {
        // Only bit 2 set: Unmapped(0,1024) Data(1024,512) Unmapped(1536,2560)
        let mut bitmap = [0x00u8; 4096];
        bitmap[0] = 0x04; // bit 2 set
        let (vhdx, data_offset, _) = create_partial_block_vhdx(&bitmap).await;

        let mut ranges = Vec::new();
        vhdx.resolve_read(0, 4096, &mut ranges).await.unwrap();

        assert_eq!(ranges.len(), 3);
        assert_eq!(
            ranges[0],
            ReadRange::Unmapped {
                guest_offset: 0,
                length: 1024, // 2 sectors
            }
        );
        assert_eq!(
            ranges[1],
            ReadRange::Data {
                guest_offset: 1024,
                length: 512,
                file_offset: data_offset + 1024,
            }
        );
        assert_eq!(
            ranges[2],
            ReadRange::Unmapped {
                guest_offset: 1536,
                length: 2560, // 5 sectors
            }
        );
    }

    #[async_test]
    async fn partial_block_first_and_last_sector() {
        // Bits 0 and 7 set in byte 0: Data(0,512) Unmapped(512,3072) Data(3584,512)
        let mut bitmap = [0x00u8; 4096];
        bitmap[0] = 0x81; // bits 0 and 7 set
        let (vhdx, data_offset, _) = create_partial_block_vhdx(&bitmap).await;

        let mut ranges = Vec::new();
        vhdx.resolve_read(0, 4096, &mut ranges).await.unwrap();

        assert_eq!(ranges.len(), 3);
        assert_eq!(
            ranges[0],
            ReadRange::Data {
                guest_offset: 0,
                length: 512,
                file_offset: data_offset,
            }
        );
        assert_eq!(
            ranges[1],
            ReadRange::Unmapped {
                guest_offset: 512,
                length: 3072, // 6 sectors
            }
        );
        assert_eq!(
            ranges[2],
            ReadRange::Data {
                guest_offset: 3584,
                length: 512,
                file_offset: data_offset + 3584,
            }
        );
    }

    #[async_test]
    async fn partial_block_read_at_offset() {
        // All bits set. Read starting at sector 4 (offset 2048 within block).
        let bitmap = [0xFFu8; 4096];
        let (vhdx, data_offset, _) = create_partial_block_vhdx(&bitmap).await;

        let mut ranges = Vec::new();
        // Read 4 sectors starting at byte offset 2048.
        vhdx.resolve_read(2048, 2048, &mut ranges).await.unwrap();

        assert_eq!(ranges.len(), 1);
        assert_eq!(
            ranges[0],
            ReadRange::Data {
                guest_offset: 2048,
                length: 2048,
                file_offset: data_offset + 2048,
            }
        );
    }

    #[async_test]
    async fn partial_block_unallocated_sbm_error() {
        // Set up a PartiallyPresent data block but leave the SBM entry as NotPresent.
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size: format::GB1,
            has_parent: true,
            ..Default::default()
        };
        create::create(&file, &mut params).await.unwrap();

        let regions = region::parse_region_tables(&file).await.unwrap();
        let bat_offset = regions.bat_offset;
        let bat = Bat::new(
            format::GB1,
            format::DEFAULT_BLOCK_SIZE,
            512,
            true,
            format::MB1 as u32,
        )
        .unwrap();
        let payload_index = bat.payload_entry_index(0);

        // Set block 0 to PartiallyPresent but do NOT set the SBM entry.
        let data_block_offset = 8 * format::MB1;
        let data_entry = BatEntry::new()
            .with_state(BatEntryState::PartiallyPresent as u8)
            .with_file_offset_mb(data_block_offset >> 20);
        file.write_at(bat_offset + payload_index as u64 * 8, data_entry.as_bytes())
            .await
            .unwrap();

        // Extend file to cover the data block offset + block size.
        let needed = data_block_offset + format::DEFAULT_BLOCK_SIZE as u64;
        file.set_file_size(needed).await.unwrap();

        // Open should fail because the PartiallyPresent block has no
        // corresponding SBM allocation.
        let result = VhdxFile::open(file).read_only().await;
        assert!(
            result.is_err(),
            "open should reject PartiallyPresent block without SBM"
        );
    }

    #[async_test]
    async fn set_sector_bitmap_bits_roundtrip(driver: DefaultDriver) {
        // Create a differencing VHDX with all-zero bitmap (all transparent).
        // This test writes SBM bits, so it needs a writable VhdxFile.
        let bitmap = [0x00u8; 4096];
        let (_, data_offset, _) = create_partial_block_vhdx(&bitmap).await;

        // Re-create the same setup but open writable.
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size: format::GB1,
            has_parent: true,
            ..Default::default()
        };
        create::create(&file, &mut params).await.unwrap();
        let regions = region::parse_region_tables(&file).await.unwrap();
        let bat_offset = regions.bat_offset;
        let bat = Bat::new(
            format::GB1,
            format::DEFAULT_BLOCK_SIZE,
            512,
            true,
            format::MB1 as u32,
        )
        .unwrap();
        let payload_index = bat.payload_entry_index(0);
        let sbm_index = bat.sector_bitmap_entry_index(0);

        let data_block_offset = 8 * format::MB1;
        let data_entry = BatEntry::new()
            .with_state(BatEntryState::PartiallyPresent as u8)
            .with_file_offset_mb(data_block_offset >> 20);
        file.write_at(bat_offset + payload_index as u64 * 8, data_entry.as_bytes())
            .await
            .unwrap();

        let sbm_block_offset = 10 * format::MB1;
        let sbm_entry = BatEntry::new()
            .with_state(BatEntryState::FullyPresent as u8)
            .with_file_offset_mb(sbm_block_offset >> 20);
        file.write_at(bat_offset + sbm_index as u64 * 8, sbm_entry.as_bytes())
            .await
            .unwrap();

        file.write_at(sbm_block_offset, &bitmap).await.unwrap();

        let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();

        // Verify initial state: sectors 0-7 are transparent.
        let mut ranges = Vec::new();
        vhdx.resolve_read(0, 4096, &mut ranges).await.unwrap();
        assert_eq!(ranges.len(), 1);
        assert_eq!(
            ranges[0],
            ReadRange::Unmapped {
                guest_offset: 0,
                length: 4096,
            }
        );

        // Set bits for sectors 0-3 (first 2048 bytes).
        vhdx.set_sector_bitmap_bits(
            0,    // virtual_offset
            2048, // length (4 sectors * 512)
            true, // set
        )
        .await
        .unwrap();

        // Now read again: first 4 sectors should be Data, last 4 Unmapped.
        let mut ranges2 = Vec::new();
        vhdx.resolve_read(0, 4096, &mut ranges2).await.unwrap();
        assert_eq!(ranges2.len(), 2);
        assert_eq!(
            ranges2[0],
            ReadRange::Data {
                guest_offset: 0,
                length: 2048,
                file_offset: data_offset,
            }
        );
        assert_eq!(
            ranges2[1],
            ReadRange::Unmapped {
                guest_offset: 2048,
                length: 2048,
            }
        );
    }
}
