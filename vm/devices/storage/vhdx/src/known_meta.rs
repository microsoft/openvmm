// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Known metadata parsing for VHDX files.
//!
//! Verifies that all required system metadata items are recognized, then
//! reads and parses the well-known items (file parameters, disk size,
//! sector sizes, page 83 data) into typed Rust values.

use crate::AsyncFile;
use crate::cache::PageCache;
use crate::error::CorruptionType;
use crate::error::OpenError;
use crate::format;
use crate::format::FileParameters;
use crate::format::FileParametersFlags;
use crate::metadata::MetadataTable;
use guid::Guid;
use zerocopy::FromBytes;

/// Parsed metadata from a VHDX file's metadata region.
pub(crate) struct KnownMetadata {
    /// Block size in bytes.
    pub block_size: u32,
    /// Whether this is a differencing disk.
    pub has_parent: bool,
    /// Whether blocks should remain allocated (fixed VHD).
    pub leave_blocks_allocated: bool,
    /// Virtual disk size in bytes.
    pub disk_size: u64,
    /// Logical sector size (512 or 4096).
    pub logical_sector_size: u32,
    /// Physical sector size (512 or 4096).
    pub physical_sector_size: u32,
    /// Page 83 data GUID.
    pub page_83_data: Guid,
}

/// Known system metadata item GUIDs that this parser understands.
const KNOWN_ITEM_IDS: &[Guid] = &[
    format::FILE_PARAMETERS_ITEM_GUID,
    format::VIRTUAL_DISK_SIZE_ITEM_GUID,
    format::PAGE_83_ITEM_GUID,
    format::CHS_PARAMETERS_ITEM_GUID,
    format::LOGICAL_SECTOR_SIZE_ITEM_GUID,
    format::PHYSICAL_SECTOR_SIZE_ITEM_GUID,
    format::PARENT_LOCATOR_ITEM_GUID,
    format::PMEM_LABEL_STORAGE_AREA_ITEM_GUID,
];

/// Verify that all required system metadata items in the table are known to
/// this parser. Unknown required items cause an error (except the incomplete
/// file marker, which has special handling).
pub(crate) fn verify_known_metadata(
    table: &MetadataTable,
    allow_incomplete: bool,
) -> Result<(), OpenError> {
    for entry in table.entries() {
        // Only check system (non-user) entries that are required.
        if entry.flags.is_user() || !entry.flags.is_required() {
            continue;
        }

        if KNOWN_ITEM_IDS.contains(&entry.item_id) {
            continue;
        }

        if entry.item_id == format::INCOMPLETE_FILE_ITEM_GUID {
            if allow_incomplete {
                continue;
            }
            return Err(CorruptionType::IncompleteFile.into());
        }

        return Err(CorruptionType::UnknownRequiredMetadata.into());
    }
    Ok(())
}

/// Read and parse all known metadata items from the file.
pub(crate) async fn read_known_metadata(
    cache: &PageCache<impl AsyncFile>,
    table: &MetadataTable,
) -> Result<KnownMetadata, OpenError> {
    // --- Logical sector size (read first, needed for disk size validation) ---
    let logical_sector_size = {
        let entry = table
            .find_entry(false, &format::LOGICAL_SECTOR_SIZE_ITEM_GUID)
            .ok_or(CorruptionType::MissingRequiredMetadata)?;

        if entry.length != 4 {
            return Err(CorruptionType::InvalidLogicalSectorSizeSize.into());
        }
        if !entry.flags.is_virtual_disk() {
            return Err(CorruptionType::LogicalSectorSizeMarkedVirtual.into());
        }

        let data = table
            .read_item(cache, false, &format::LOGICAL_SECTOR_SIZE_ITEM_GUID)
            .await?;
        let value = u32::from_le_bytes(data.try_into().unwrap());
        if value != 512 && value != 4096 {
            return Err(CorruptionType::InvalidLogicalSectorSize.into());
        }
        value
    };

    // --- File parameters ---
    let (block_size, has_parent, leave_blocks_allocated) = {
        let entry = table
            .find_entry(false, &format::FILE_PARAMETERS_ITEM_GUID)
            .ok_or(CorruptionType::MissingRequiredMetadata)?;

        if entry.length as usize != size_of::<FileParameters>() {
            return Err(CorruptionType::InvalidFileParameterSize.into());
        }
        if entry.flags.is_virtual_disk() {
            return Err(CorruptionType::FileParametersMarkedVirtual.into());
        }

        let data = table
            .read_item(cache, false, &format::FILE_PARAMETERS_ITEM_GUID)
            .await?;
        let params = FileParameters::read_from_bytes(&data)
            .map_err(|_| CorruptionType::InvalidFileParameterSize)?;
        let supported = u32::from(
            FileParametersFlags::new()
                .with_leave_blocks_allocated(true)
                .with_has_parent(true),
        );
        if u32::from(params.flags) & !supported != 0 {
            return Err(CorruptionType::ReservedFileParametersFieldNonzero.into());
        }

        let bs = params.block_size;
        if !bs.is_power_of_two()
            || (bs as u64) < format::MB1
            || bs as u64 > format::MAXIMUM_BLOCK_SIZE
        {
            return Err(CorruptionType::InvalidBlockSize.into());
        }

        (
            bs,
            params.flags.has_parent(),
            params.flags.leave_blocks_allocated(),
        )
    };

    // --- Virtual disk size ---
    let disk_size = {
        let entry = table
            .find_entry(false, &format::VIRTUAL_DISK_SIZE_ITEM_GUID)
            .ok_or(CorruptionType::MissingRequiredMetadata)?;

        if entry.length != 8 {
            return Err(CorruptionType::InvalidDiskSize.into());
        }
        if !entry.flags.is_virtual_disk() {
            return Err(CorruptionType::DiskMarkedVirtual.into());
        }

        let data = table
            .read_item(cache, false, &format::VIRTUAL_DISK_SIZE_ITEM_GUID)
            .await?;
        let value = u64::from_le_bytes(data.try_into().unwrap());
        if value == 0
            || value > format::MAXIMUM_DISK_SIZE
            || !value.is_multiple_of(logical_sector_size as u64)
        {
            return Err(CorruptionType::InvalidDiskSize.into());
        }
        value
    };

    // --- Physical sector size ---
    let physical_sector_size = {
        let entry = table
            .find_entry(false, &format::PHYSICAL_SECTOR_SIZE_ITEM_GUID)
            .ok_or(CorruptionType::MissingRequiredMetadata)?;

        if entry.length != 4 {
            return Err(CorruptionType::InvalidSectorSize.into());
        }
        if !entry.flags.is_virtual_disk() {
            return Err(CorruptionType::InvalidSectorSize.into());
        }

        let data = table
            .read_item(cache, false, &format::PHYSICAL_SECTOR_SIZE_ITEM_GUID)
            .await?;
        let value = u32::from_le_bytes(data.try_into().unwrap());
        if value != 512 && value != 4096 {
            return Err(CorruptionType::InvalidSectorSize.into());
        }
        value
    };

    // --- Page 83 data ---
    let page_83_data = {
        let entry = table
            .find_entry(false, &format::PAGE_83_ITEM_GUID)
            .ok_or(CorruptionType::MissingRequiredMetadata)?;

        if entry.length != 16 {
            return Err(CorruptionType::MissingRequiredMetadata.into());
        }
        if !entry.flags.is_virtual_disk() {
            return Err(CorruptionType::MissingRequiredMetadata.into());
        }

        let data = table
            .read_item(cache, false, &format::PAGE_83_ITEM_GUID)
            .await?;
        Guid::read_from_bytes(&data).map_err(|_| CorruptionType::MissingRequiredMetadata)?
    };

    Ok(KnownMetadata {
        block_size,
        has_parent,
        leave_blocks_allocated,
        disk_size,
        logical_sector_size,
        physical_sector_size,
        page_83_data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AsyncFileExt;
    use crate::cache::PageCache;
    use crate::error::OpenErrorInner;
    use crate::metadata::METADATA_TAG;
    use crate::region;
    use crate::tests::support::InMemoryFile;
    use pal_async::async_test;
    use std::sync::Arc;
    use zerocopy::IntoBytes;

    fn metadata_cache(file: InMemoryFile, metadata_offset: u64) -> PageCache<InMemoryFile> {
        let mut cache = PageCache::new(Arc::new(file), None, None, 0);
        cache.register_tag(METADATA_TAG, metadata_offset);
        cache
    }

    #[async_test]
    async fn read_known_metadata_from_created_file() {
        let (file, params) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let regions = region::parse_region_tables(&file).await.unwrap();
        let table = MetadataTable::read(&file, regions.metadata_offset, regions.metadata_length)
            .await
            .unwrap();
        let cache = metadata_cache(file, regions.metadata_offset);

        let meta = read_known_metadata(&cache, &table).await.unwrap();

        assert_eq!(meta.disk_size, format::GB1);
        assert_eq!(meta.block_size, params.block_size);
        assert_eq!(meta.logical_sector_size, params.logical_sector_size);
        assert_eq!(meta.physical_sector_size, params.physical_sector_size);
        assert!(!meta.has_parent);
        assert!(!meta.leave_blocks_allocated);
        assert_ne!(meta.page_83_data, Guid::ZERO);
    }

    #[async_test]
    async fn verify_known_metadata_all_known() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let regions = region::parse_region_tables(&file).await.unwrap();
        let table = MetadataTable::read(&file, regions.metadata_offset, regions.metadata_length)
            .await
            .unwrap();

        // All standard entries should be recognized.
        verify_known_metadata(&table, false).unwrap();
    }

    #[async_test]
    async fn verify_unknown_required_item() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let regions = region::parse_region_tables(&file).await.unwrap();

        // Add a fake required system metadata entry to the table.
        let mut buf = vec![0u8; format::METADATA_TABLE_SIZE as usize];
        file.read_at(regions.metadata_offset, &mut buf)
            .await
            .unwrap();

        let mut header = format::MetadataTableHeader::read_from_prefix(&buf)
            .unwrap()
            .0
            .clone();
        let old_count = header.entry_count;
        header.entry_count = old_count + 1;
        let h_bytes = header.as_bytes();
        buf[..h_bytes.len()].copy_from_slice(h_bytes);

        // Add a fake entry with unknown GUID.
        let fake_guid = guid::guid!("deadbeef-dead-beef-dead-beefdeadbeef");
        let fake_entry = format::MetadataTableEntry {
            item_id: fake_guid,
            offset: 0,
            length: 0,
            flags: format::MetadataTableEntryFlags::new().with_is_required(true),
            reserved2: 0,
        };
        let entry_start = size_of::<format::MetadataTableHeader>();
        let entry_size = size_of::<format::MetadataTableEntry>();
        let off = entry_start + old_count as usize * entry_size;
        let e_bytes = fake_entry.as_bytes();
        buf[off..off + e_bytes.len()].copy_from_slice(e_bytes);

        file.write_at(regions.metadata_offset, &buf).await.unwrap();

        let table = MetadataTable::read(&file, regions.metadata_offset, regions.metadata_length)
            .await
            .unwrap();

        let result = verify_known_metadata(&table, false);
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::Corrupt(
                CorruptionType::UnknownRequiredMetadata
            )))
        ));
    }

    #[async_test]
    async fn verify_incomplete_file() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let regions = region::parse_region_tables(&file).await.unwrap();

        // Add the incomplete file marker.
        let mut buf = vec![0u8; format::METADATA_TABLE_SIZE as usize];
        file.read_at(regions.metadata_offset, &mut buf)
            .await
            .unwrap();

        let mut header = format::MetadataTableHeader::read_from_prefix(&buf)
            .unwrap()
            .0
            .clone();
        let old_count = header.entry_count;
        header.entry_count = old_count + 1;
        let h_bytes = header.as_bytes();
        buf[..h_bytes.len()].copy_from_slice(h_bytes);

        let incomplete_entry = format::MetadataTableEntry {
            item_id: format::INCOMPLETE_FILE_ITEM_GUID,
            offset: 0,
            length: 0,
            flags: format::MetadataTableEntryFlags::new().with_is_required(true),
            reserved2: 0,
        };
        let entry_start = size_of::<format::MetadataTableHeader>();
        let entry_size = size_of::<format::MetadataTableEntry>();
        let off = entry_start + old_count as usize * entry_size;
        let e_bytes = incomplete_entry.as_bytes();
        buf[off..off + e_bytes.len()].copy_from_slice(e_bytes);

        file.write_at(regions.metadata_offset, &buf).await.unwrap();

        let table = MetadataTable::read(&file, regions.metadata_offset, regions.metadata_length)
            .await
            .unwrap();

        // With allow_incomplete=false, should fail.
        let result = verify_known_metadata(&table, false);
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::Corrupt(
                CorruptionType::IncompleteFile
            )))
        ));

        // With allow_incomplete=true, should pass.
        verify_known_metadata(&table, true).unwrap();
    }

    #[async_test]
    async fn validate_block_size_power_of_two() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let regions = region::parse_region_tables(&file).await.unwrap();
        let table = MetadataTable::read(&file, regions.metadata_offset, regions.metadata_length)
            .await
            .unwrap();

        // Overwrite file parameters with a non-power-of-2 block size.
        let entry = table
            .find_entry(false, &format::FILE_PARAMETERS_ITEM_GUID)
            .unwrap();
        let item_offset = regions.metadata_offset + entry.offset as u64;

        let bad_params = FileParameters {
            block_size: 3 * format::MB1 as u32, // not power of 2
            flags: FileParametersFlags::new(),
        };
        file.write_at(item_offset, bad_params.as_bytes())
            .await
            .unwrap();

        let cache = metadata_cache(file, regions.metadata_offset);
        let result = read_known_metadata(&cache, &table).await;
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::Corrupt(
                CorruptionType::InvalidBlockSize
            )))
        ));
    }

    #[async_test]
    async fn validate_file_parameters_reserved_flags() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let regions = region::parse_region_tables(&file).await.unwrap();
        let table = MetadataTable::read(&file, regions.metadata_offset, regions.metadata_length)
            .await
            .unwrap();

        let entry = table
            .find_entry(false, &format::FILE_PARAMETERS_ITEM_GUID)
            .unwrap();
        let item_offset = regions.metadata_offset + entry.offset as u64;

        let params = FileParameters {
            block_size: format::DEFAULT_BLOCK_SIZE,
            flags: FileParametersFlags::new(),
        };
        let mut bytes = params.as_bytes().to_vec();
        bytes[4..8].copy_from_slice(&4_u32.to_le_bytes());
        file.write_at(item_offset, &bytes).await.unwrap();

        let cache = metadata_cache(file, regions.metadata_offset);
        let result = read_known_metadata(&cache, &table).await;
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::Corrupt(
                CorruptionType::ReservedFileParametersFieldNonzero
            )))
        ));
    }

    #[async_test]
    async fn validate_sector_sizes() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let regions = region::parse_region_tables(&file).await.unwrap();
        let table = MetadataTable::read(&file, regions.metadata_offset, regions.metadata_length)
            .await
            .unwrap();

        // Overwrite logical sector size with an invalid value.
        let entry = table
            .find_entry(false, &format::LOGICAL_SECTOR_SIZE_ITEM_GUID)
            .unwrap();
        let item_offset = regions.metadata_offset + entry.offset as u64;

        let bad_value: u32 = 1024; // not 512 or 4096
        file.write_at(item_offset, &bad_value.to_le_bytes())
            .await
            .unwrap();

        let cache = metadata_cache(file, regions.metadata_offset);
        let result = read_known_metadata(&cache, &table).await;
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::Corrupt(
                CorruptionType::InvalidLogicalSectorSize
            )))
        ));
    }
}
