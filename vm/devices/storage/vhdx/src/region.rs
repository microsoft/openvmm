// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Region table parsing and validation for VHDX files.
//!
//! Reads both region tables, validates their signatures and CRC-32C checksums,
//! identifies BAT and metadata regions, and checks for overlaps and duplicates.

use crate::AsyncFile;
use crate::cache::PageCache;
use crate::error::CorruptionType;
use crate::error::OpenError;
use crate::error::OpenErrorInner;
use crate::format;
use crate::format::RegionTableEntry;
use crate::format::RegionTableHeader;
use crate::log_task::LogData;
use crate::log_task::Lsn;
use std::sync::Arc;
use zerocopy::FromBytes;

/// Parsed region table data.
pub(crate) struct ParsedRegions<B> {
    /// File offset of the BAT region.
    pub bat_offset: u64,
    /// Length of the BAT region in bytes.
    pub bat_length: u32,
    /// File offset of the metadata region.
    pub metadata_offset: u64,
    /// Length of the metadata region in bytes.
    pub metadata_length: u32,
    /// The validated region table bytes. Present only when the two on-disk
    /// copies don't match and need rewriting.
    pub rewrite_data: Option<B>,
}

/// Read and validate a single 64 KiB region table from the file.
/// Returns the raw bytes if valid, or `None` if the table is corrupt.
async fn read_and_validate_region_table<F: AsyncFile>(
    file: &F,
    offset: u64,
) -> Result<Option<F::Buffer>, OpenError> {
    let buf = file.alloc_buffer(format::REGION_TABLE_SIZE as usize);
    let buf = file
        .read_into(offset, buf)
        .await
        .map_err(OpenErrorInner::Io)?;
    let buf_ref = buf.as_ref();

    // Check signature.
    let header = match RegionTableHeader::read_from_prefix(buf_ref) {
        Ok((h, _)) => h,
        Err(_) => return Ok(None),
    };
    if header.signature != format::REGION_TABLE_SIGNATURE {
        return Ok(None);
    }

    // Validate CRC-32C checksum (checksum field is at byte offset 4).
    if !format::validate_checksum(buf_ref, 4) {
        return Ok(None);
    }

    Ok(Some(buf))
}

/// Read both region tables from the file, validate, and extract BAT/metadata
/// region locations.
pub(crate) async fn parse_region_tables<F: AsyncFile>(
    file: &F,
) -> Result<ParsedRegions<F::Buffer>, OpenError> {
    let table1 = read_and_validate_region_table(file, format::REGION_TABLE_OFFSET).await?;
    let table2 = read_and_validate_region_table(file, format::ALT_REGION_TABLE_OFFSET).await?;

    let (table, needs_rewrite) = match (table1, table2) {
        (Some(t1), Some(t2)) => {
            let needs_rewrite = t1.as_ref() != t2.as_ref();
            (t1, needs_rewrite)
        }
        (Some(t1), None) => (t1, true),
        (None, Some(t2)) => (t2, true),
        (None, None) => return Err(CorruptionType::RegionTablesBothCorrupt.into()),
    };

    // Parse the header to get entry count.
    let header = RegionTableHeader::read_from_prefix(table.as_ref())
        .unwrap()
        .0
        .clone();

    if header.entry_count as u64 > format::REGION_TABLE_MAX_ENTRY_COUNT {
        return Err(CorruptionType::InvalidEntryCountInRegionTable.into());
    }
    if header.reserved != 0 {
        return Err(CorruptionType::ReservedRegionTableFieldNonzero.into());
    }

    // Parse all entries.
    let entry_size = size_of::<RegionTableEntry>();
    let header_size = size_of::<RegionTableHeader>();
    let mut entries = Vec::with_capacity(header.entry_count as usize);
    for i in 0..header.entry_count as usize {
        let offset = header_size + i * entry_size;
        let entry = RegionTableEntry::read_from_prefix(&table.as_ref()[offset..])
            .unwrap()
            .0
            .clone();

        let supported = u32::from(format::RegionTableEntryFlags::new().with_required(true));
        if u32::from(entry.flags) & !supported != 0 {
            return Err(CorruptionType::ReservedRegionTableFieldNonzero.into());
        }
        entries.push(entry);
    }

    // Sort by GUID for duplicate detection.
    entries.sort_by_key(|a| a.guid);

    // Check for duplicate GUIDs.
    for i in 1..entries.len() {
        if entries[i].guid == entries[i - 1].guid {
            return Err(CorruptionType::DuplicateRegionEntry.into());
        }
    }

    // Validate each entry's offset and length.
    for entry in &entries {
        if entry.length == 0 {
            return Err(CorruptionType::OffsetOrLengthInRegionTable.into());
        }
        if !entry.file_offset.is_multiple_of(format::REGION_ALIGNMENT)
            || !(entry.length as u64).is_multiple_of(format::REGION_ALIGNMENT)
        {
            return Err(CorruptionType::OffsetOrLengthInRegionTable.into());
        }
    }

    // Check for overlapping regions by sorting by offset.
    let mut by_offset: Vec<(u64, u64)> = entries
        .iter()
        .map(|e| (e.file_offset, e.length as u64))
        .collect();
    // Also include the 1 MiB header area as a reserved region.
    by_offset.push((0, format::HEADER_AREA_SIZE));
    by_offset.sort_by_key(|&(offset, _)| offset);

    for i in 1..by_offset.len() {
        let prev_end = by_offset[i - 1]
            .0
            .checked_add(by_offset[i - 1].1)
            .ok_or(CorruptionType::OffsetOrLengthInRegionTable)?;
        if prev_end > by_offset[i].0 {
            return Err(CorruptionType::OffsetOrLengthInRegionTable.into());
        }
    }

    // Identify known regions.
    let mut bat_offset = None;
    let mut bat_length = None;
    let mut metadata_offset = None;
    let mut metadata_length = None;

    for entry in &entries {
        if entry.guid == format::BAT_REGION_GUID {
            bat_offset = Some(entry.file_offset);
            bat_length = Some(entry.length);
        } else if entry.guid == format::METADATA_REGION_GUID {
            metadata_offset = Some(entry.file_offset);
            metadata_length = Some(entry.length);
        } else if entry.flags.required() {
            return Err(CorruptionType::UnknownRequiredRegion.into());
        }
        // Unknown non-required regions are silently ignored.
    }

    let bat_offset = bat_offset.ok_or(CorruptionType::MissingBatOrMetadataRegion)?;
    let bat_length = bat_length.ok_or(CorruptionType::MissingBatOrMetadataRegion)?;
    let metadata_offset = metadata_offset.ok_or(CorruptionType::MissingBatOrMetadataRegion)?;
    let metadata_length = metadata_length.ok_or(CorruptionType::MissingBatOrMetadataRegion)?;

    Ok(ParsedRegions {
        bat_offset,
        bat_length,
        metadata_offset,
        metadata_length,
        rewrite_data: if needs_rewrite {
            Some(table.clone())
        } else {
            None
        },
    })
}

/// Write the region table to both on-disk slots via the write-ahead log.
///
/// Called during [`VhdxBuilder::writable`](crate::open::VhdxBuilder::writable)
/// when one region table was corrupt or the two copies didn't match. Acquires
/// log permits, sends the pages through [`PageCache::commit_raw`], and returns
/// the LSN. The caller must wait for the LSN and flush to make the writes
/// durable.
pub(crate) async fn rewrite_region_tables<F: AsyncFile>(
    cache: &PageCache<F>,
    log_permits: &crate::log_permits::LogPermits,
    table: F::Buffer,
) -> Result<Lsn, crate::error::PipelineFailed> {
    assert_eq!(
        table.as_ref().len(),
        format::REGION_TABLE_SIZE as usize,
        "region table must be exactly {} bytes",
        format::REGION_TABLE_SIZE
    );

    let log_data_page_size = format::LOG_SECTOR_SIZE as usize;
    let pages_per_table = format::REGION_TABLE_SIZE as usize / log_data_page_size;
    let total_pages = pages_per_table * 2;
    let table = Arc::new(table);
    let mut pages = Vec::with_capacity(2);

    for base_offset in [format::REGION_TABLE_OFFSET, format::ALT_REGION_TABLE_OFFSET] {
        pages.push(LogData::new(base_offset, table.clone()));
    }

    log_permits.acquire(total_pages).await?;
    Ok(cache.commit_raw(pages, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AsyncFileExt;
    use crate::error::OpenErrorInner;
    use crate::open::VhdxFile;
    use crate::tests::support::InMemoryFile;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use zerocopy::IntoBytes;

    #[async_test]
    async fn parse_valid_region_tables() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let regions = parse_region_tables(&file).await.unwrap();

        // Metadata at 2 MiB, BAT at 3 MiB (based on create layout).
        assert_eq!(regions.metadata_offset, 2 * format::MB1);
        assert_eq!(
            regions.metadata_length,
            format::DEFAULT_METADATA_REGION_SIZE
        );
        assert_eq!(regions.bat_offset, 3 * format::MB1);
        assert!(regions.rewrite_data.is_none());
    }

    #[async_test]
    async fn parse_one_corrupt_table() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;

        // Corrupt the first region table's CRC.
        let mut buf = vec![0u8; format::REGION_TABLE_SIZE as usize];
        file.read_at(format::REGION_TABLE_OFFSET, &mut buf)
            .await
            .unwrap();
        buf[10] ^= 0xFF;
        file.write_at(format::REGION_TABLE_OFFSET, &buf)
            .await
            .unwrap();

        let regions = parse_region_tables(&file).await.unwrap();
        assert!(regions.rewrite_data.is_some());
        // Should still parse successfully using table 2.
        assert_eq!(regions.metadata_offset, 2 * format::MB1);
    }

    #[async_test]
    async fn parse_both_corrupt() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;

        // Corrupt both region tables.
        for offset in [format::REGION_TABLE_OFFSET, format::ALT_REGION_TABLE_OFFSET] {
            let mut buf = vec![0u8; format::REGION_TABLE_SIZE as usize];
            file.read_at(offset, &mut buf).await.unwrap();
            buf[10] ^= 0xFF;
            file.write_at(offset, &buf).await.unwrap();
        }

        let result = parse_region_tables(&file).await;
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::Corrupt(
                CorruptionType::RegionTablesBothCorrupt
            )))
        ));
    }

    /// Helper to build a custom region table and write it to both locations.
    async fn write_custom_region_table(file: &InMemoryFile, entries: &[RegionTableEntry]) {
        let mut buf = vec![0u8; format::REGION_TABLE_SIZE as usize];
        let header = RegionTableHeader {
            signature: format::REGION_TABLE_SIGNATURE,
            checksum: 0,
            entry_count: entries.len() as u32,
            reserved: 0,
        };
        let h_bytes = header.as_bytes();
        buf[..h_bytes.len()].copy_from_slice(h_bytes);

        let entry_start = size_of::<RegionTableHeader>();
        for (i, entry) in entries.iter().enumerate() {
            let off = entry_start + i * size_of::<RegionTableEntry>();
            let e_bytes = entry.as_bytes();
            buf[off..off + e_bytes.len()].copy_from_slice(e_bytes);
        }

        let crc = format::compute_checksum(&buf, 4);
        buf[4..8].copy_from_slice(&crc.to_le_bytes());

        file.write_at(format::REGION_TABLE_OFFSET, &buf)
            .await
            .unwrap();
        file.write_at(format::ALT_REGION_TABLE_OFFSET, &buf)
            .await
            .unwrap();
    }

    #[async_test]
    async fn parse_missing_bat_region() {
        let file = InMemoryFile::new(format::HEADER_AREA_SIZE);
        // Only metadata region, no BAT.
        let entries = vec![RegionTableEntry {
            guid: format::METADATA_REGION_GUID,
            file_offset: 2 * format::MB1,
            length: format::MB1 as u32,
            flags: format::RegionTableEntryFlags::new().with_required(true),
        }];
        write_custom_region_table(&file, &entries).await;

        let result = parse_region_tables(&file).await;
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::Corrupt(
                CorruptionType::MissingBatOrMetadataRegion
            )))
        ));
    }

    #[async_test]
    async fn parse_duplicate_region() {
        let file = InMemoryFile::new(format::HEADER_AREA_SIZE);
        let entries = vec![
            RegionTableEntry {
                guid: format::BAT_REGION_GUID,
                file_offset: 2 * format::MB1,
                length: format::MB1 as u32,
                flags: format::RegionTableEntryFlags::new().with_required(true),
            },
            RegionTableEntry {
                guid: format::BAT_REGION_GUID,
                file_offset: 3 * format::MB1,
                length: format::MB1 as u32,
                flags: format::RegionTableEntryFlags::new().with_required(true),
            },
        ];
        write_custom_region_table(&file, &entries).await;

        let result = parse_region_tables(&file).await;
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::Corrupt(
                CorruptionType::DuplicateRegionEntry
            )))
        ));
    }

    async fn corrupt_region_table_bytes(
        file: &InMemoryFile,
        entries: &[RegionTableEntry],
        corrupt: impl FnOnce(&mut [u8]),
    ) {
        write_custom_region_table(file, entries).await;

        let mut buf = vec![0u8; format::REGION_TABLE_SIZE as usize];
        file.read_at(format::REGION_TABLE_OFFSET, &mut buf)
            .await
            .unwrap();
        corrupt(&mut buf);
        let crc = format::compute_checksum(&buf, 4);
        buf[4..8].copy_from_slice(&crc.to_le_bytes());

        file.write_at(format::REGION_TABLE_OFFSET, &buf)
            .await
            .unwrap();
        file.write_at(format::ALT_REGION_TABLE_OFFSET, &buf)
            .await
            .unwrap();
    }

    #[async_test]
    async fn parse_region_table_with_nonzero_reserved_header() {
        let file = InMemoryFile::new(format::HEADER_AREA_SIZE);
        let entries = vec![
            RegionTableEntry {
                guid: format::BAT_REGION_GUID,
                file_offset: 2 * format::MB1,
                length: format::MB1 as u32,
                flags: format::RegionTableEntryFlags::new().with_required(true),
            },
            RegionTableEntry {
                guid: format::METADATA_REGION_GUID,
                file_offset: 3 * format::MB1,
                length: format::MB1 as u32,
                flags: format::RegionTableEntryFlags::new().with_required(true),
            },
        ];
        corrupt_region_table_bytes(&file, &entries, |buf| {
            buf[12..16].copy_from_slice(&1_u32.to_le_bytes());
        })
        .await;

        let result = parse_region_tables(&file).await;
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::Corrupt(
                CorruptionType::ReservedRegionTableFieldNonzero
            )))
        ));
    }

    #[async_test]
    async fn parse_region_table_with_reserved_entry_flags() {
        let file = InMemoryFile::new(format::HEADER_AREA_SIZE);
        let entries = vec![
            RegionTableEntry {
                guid: format::BAT_REGION_GUID,
                file_offset: 2 * format::MB1,
                length: format::MB1 as u32,
                flags: format::RegionTableEntryFlags::new().with_required(true),
            },
            RegionTableEntry {
                guid: format::METADATA_REGION_GUID,
                file_offset: 3 * format::MB1,
                length: format::MB1 as u32,
                flags: format::RegionTableEntryFlags::new().with_required(true),
            },
        ];
        corrupt_region_table_bytes(&file, &entries, |buf| {
            let entry_flags_offset = size_of::<RegionTableHeader>() + 28;
            buf[entry_flags_offset..entry_flags_offset + 4].copy_from_slice(&3_u32.to_le_bytes());
        })
        .await;

        let result = parse_region_tables(&file).await;
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::Corrupt(
                CorruptionType::ReservedRegionTableFieldNonzero
            )))
        ));
    }

    #[async_test]
    async fn parse_overlapping_regions() {
        let file = InMemoryFile::new(format::HEADER_AREA_SIZE);
        // Two regions that overlap at the 2 MiB mark.
        let entries = vec![
            RegionTableEntry {
                guid: format::BAT_REGION_GUID,
                file_offset: 2 * format::MB1,
                length: 2 * format::MB1 as u32,
                flags: format::RegionTableEntryFlags::new().with_required(true),
            },
            RegionTableEntry {
                guid: format::METADATA_REGION_GUID,
                file_offset: 3 * format::MB1,
                length: format::MB1 as u32,
                flags: format::RegionTableEntryFlags::new().with_required(true),
            },
        ];
        write_custom_region_table(&file, &entries).await;

        let result = parse_region_tables(&file).await;
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::Corrupt(
                CorruptionType::OffsetOrLengthInRegionTable
            )))
        ));
    }

    #[async_test]
    async fn rewrite_repairs_corrupt_table(driver: DefaultDriver) {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;

        // Corrupt the first region table.
        let mut buf = vec![0u8; format::REGION_TABLE_SIZE as usize];
        file.read_at(format::REGION_TABLE_OFFSET, &mut buf)
            .await
            .unwrap();
        buf[10] ^= 0xFF;
        file.write_at(format::REGION_TABLE_OFFSET, &buf)
            .await
            .unwrap();

        // Opening writable should detect and repair the mismatch via the log.
        let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
        let file_ref = vhdx.file.clone();
        vhdx.close().await.unwrap();

        // Parse again — both should match now.
        let regions2 = parse_region_tables(&*file_ref).await.unwrap();
        assert!(
            regions2.rewrite_data.is_none(),
            "tables should match after rewrite"
        );

        // Verify both on-disk copies are identical.
        let mut t1 = vec![0u8; format::REGION_TABLE_SIZE as usize];
        let mut t2 = vec![0u8; format::REGION_TABLE_SIZE as usize];
        file_ref
            .read_at(format::REGION_TABLE_OFFSET, &mut t1)
            .await
            .unwrap();
        file_ref
            .read_at(format::ALT_REGION_TABLE_OFFSET, &mut t2)
            .await
            .unwrap();
        assert_eq!(t1, t2, "both region tables should be identical");
    }
}
