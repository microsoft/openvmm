// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VHDX file creation.
//!
//! Writes a valid, empty VHDX file (file identifier, dual headers, dual
//! region tables, metadata table, and empty BAT) to an [`AsyncFile`].

use crate::AsyncFile;
use crate::error::CreateError;
use crate::error::InvalidFormatReason;
use crate::format;
use crate::format::FileIdentifier;
use crate::format::FileParameters;
use crate::format::FileParametersFlags;
use crate::format::Header;
use crate::format::MetadataTableEntry;
use crate::format::MetadataTableEntryFlags;
use crate::format::MetadataTableHeader;
use crate::format::RegionTableEntry;
use crate::format::RegionTableEntryFlags;
use crate::format::RegionTableHeader;
use guid::Guid;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// Parameters for creating a new VHDX file.
pub struct CreateParams {
    /// Virtual disk size in bytes. Must be a multiple of `logical_sector_size`
    /// and at most 64 TiB.
    pub disk_size: u64,

    /// Block size in bytes. Must be a multiple of 1 MiB and at most 256 MiB.
    /// Default: 2 MiB.
    pub block_size: u32,

    /// Logical sector size. Must be 512 or 4096. Default: 512.
    pub logical_sector_size: u32,

    /// Physical sector size. Must be 512 or 4096. Default: 512.
    pub physical_sector_size: u32,

    /// Whether this is a differencing disk (has a parent).
    pub has_parent: bool,

    /// Block alignment for the data region. 0 means no special alignment.
    /// If non-zero, must be a power of 2.
    pub block_alignment: u32,

    /// If true, create the file in an incomplete state
    /// (adds an "incomplete file" metadata item that prevents open).
    pub create_incomplete: bool,

    /// If true, mark all blocks as allocated (fixed VHD).
    pub is_fully_allocated: bool,

    /// Data write GUID. If zero GUID, a random one will be generated.
    /// Callers can supply a specific GUID for re-parenting workflows.
    pub data_write_guid: Guid,

    /// Page 83 SCSI identifier. If zero GUID, a random one will be generated.
    pub page_83_data: Guid,
}

impl Default for CreateParams {
    fn default() -> Self {
        Self {
            disk_size: 0,
            block_size: 0,
            logical_sector_size: 0,
            physical_sector_size: 0,
            has_parent: false,
            block_alignment: 0,
            create_incomplete: false,
            is_fully_allocated: false,
            data_write_guid: Guid::ZERO,
            page_83_data: Guid::ZERO,
        }
    }
}

/// Integer ceiling division (a / b, rounded up). Panics if b == 0.
pub(crate) fn ceil_div(a: u64, b: u64) -> u64 {
    a.div_ceil(b)
}

/// Round `value` up to the next multiple of `alignment`.
/// `alignment` must be a power of 2.
pub(crate) fn round_up(value: u64, alignment: u64) -> u64 {
    (value + alignment - 1) & !(alignment - 1)
}

/// Compute the chunk ratio (number of data blocks per sector bitmap block).
pub(crate) fn chunk_block_count(block_size: u32, sector_size: u32) -> u32 {
    let sectors_per_block = block_size / sector_size;
    (format::SECTORS_PER_CHUNK / sectors_per_block as u64) as u32
}

/// Create a new, empty VHDX file.
///
/// Writes file identifier, dual headers, dual region tables, metadata
/// table with standard metadata items, and an empty BAT to the provided
/// file. The file is truncated/extended to the required size.
///
/// `params` is updated in place with defaults filled in (e.g. zero
/// `block_size` becomes 2 MiB, zero GUIDs become random).
pub async fn create(file: &impl AsyncFile, params: &mut CreateParams) -> Result<(), CreateError> {
    // --- Validate and default parameters ---

    if params.logical_sector_size == 0 {
        params.logical_sector_size = format::DEFAULT_SECTOR_SIZE;
    }
    if params.logical_sector_size != 512 && params.logical_sector_size != 4096 {
        return Err(CreateError::InvalidFormat(
            InvalidFormatReason::InvalidLogicalSectorSize,
        ));
    }

    if params.physical_sector_size == 0 {
        params.physical_sector_size = format::DEFAULT_SECTOR_SIZE;
    }
    if params.physical_sector_size != 512 && params.physical_sector_size != 4096 {
        return Err(CreateError::InvalidFormat(
            InvalidFormatReason::InvalidPhysicalSectorSize,
        ));
    }

    if params.disk_size == 0 {
        return Err(CreateError::InvalidFormat(
            InvalidFormatReason::DiskSizeZero,
        ));
    }
    if !params
        .disk_size
        .is_multiple_of(params.logical_sector_size as u64)
    {
        return Err(CreateError::InvalidFormat(
            InvalidFormatReason::DiskSizeNotAligned,
        ));
    }
    if params.disk_size > format::MAXIMUM_DISK_SIZE {
        return Err(CreateError::InvalidFormat(
            InvalidFormatReason::DiskSizeTooLarge,
        ));
    }

    if params.block_size == 0 {
        params.block_size = format::DEFAULT_BLOCK_SIZE;
    }
    if !(params.block_size as u64).is_multiple_of(format::REGION_ALIGNMENT) {
        return Err(CreateError::InvalidFormat(
            InvalidFormatReason::BlockSizeNotAligned,
        ));
    }
    if params.block_size as u64 > format::MAXIMUM_BLOCK_SIZE {
        return Err(CreateError::InvalidFormat(
            InvalidFormatReason::BlockSizeTooLarge,
        ));
    }

    if params.block_alignment != 0 && !params.block_alignment.is_power_of_two() {
        return Err(CreateError::InvalidFormat(
            InvalidFormatReason::BlockAlignmentNotPowerOfTwo,
        ));
    }

    // Generate a random page 83 GUID if not provided.
    if params.page_83_data == Guid::ZERO {
        params.page_83_data = Guid::new_random();
    }

    // --- Compute BAT size ---

    let data_block_count = ceil_div(params.disk_size, params.block_size as u64);
    let chunk_ratio = chunk_block_count(params.block_size, params.logical_sector_size);

    if chunk_ratio == 0 {
        return Err(CreateError::InvalidFormat(
            InvalidFormatReason::InvalidChunkRatio,
        ));
    }

    let sector_bitmap_block_count = ceil_div(data_block_count, chunk_ratio as u64);

    let bat_entry_count = if params.has_parent {
        sector_bitmap_block_count * (chunk_ratio as u64 + 1)
    } else {
        data_block_count + data_block_count.saturating_sub(1) / chunk_ratio as u64
    };

    if bat_entry_count > format::ABSOLUTE_MAXIMUM_BAT_ENTRY_COUNT {
        return Err(CreateError::InvalidFormat(
            InvalidFormatReason::BatEntryCountTooLarge,
        ));
    }

    let bat_sector_count = ceil_div(bat_entry_count, format::ENTRIES_PER_BAT_PAGE);
    let bat_length = round_up(
        bat_sector_count * format::CACHE_PAGE_SIZE,
        format::REGION_ALIGNMENT,
    );

    if bat_length > format::MAXIMUM_BAT_SIZE {
        return Err(CreateError::InvalidFormat(
            InvalidFormatReason::BatSizeTooLarge,
        ));
    }

    // --- Region layout ---

    let log_offset = format::HEADER_AREA_SIZE;
    let log_length = format::DEFAULT_LOG_SIZE as u64;
    let metadata_offset = log_offset + log_length;
    let metadata_length = format::DEFAULT_METADATA_REGION_SIZE as u64;
    let bat_offset = metadata_offset + metadata_length;

    // --- Build the 1 MiB header area buffer ---

    let mut buf = file.alloc_buffer(format::HEADER_AREA_SIZE as usize);
    let buf_bytes = buf.as_mut();

    // File identifier at offset 0.
    let mut ident = FileIdentifier::new_zeroed();
    ident.signature = format::FILE_IDENTIFIER_SIGNATURE;
    buf_bytes[..size_of::<FileIdentifier>()].copy_from_slice(ident.as_bytes());

    // Generate random GUIDs for the headers.
    let file_write_guid = Guid::new_random();
    if params.data_write_guid == Guid::ZERO {
        params.data_write_guid = Guid::new_random();
    }
    let data_write_guid = params.data_write_guid;

    // Header 1 (sequence number 0).
    let mut header = Header::new_zeroed();
    header.signature = format::HEADER_SIGNATURE;
    header.sequence_number = 0;
    header.file_write_guid = file_write_guid;
    header.data_write_guid = data_write_guid;
    header.log_guid = Guid::ZERO;
    header.log_version = format::LOG_VERSION;
    header.version = format::VERSION_1;
    header.log_offset = log_offset;
    header.log_length = log_length as u32;
    header.checksum = 0;

    // Serialize header 1, compute checksum, update.
    let h1_start = format::HEADER_OFFSET_1 as usize;
    let h1_end = h1_start + size_of::<Header>();
    buf_bytes[h1_start..h1_end].copy_from_slice(header.as_bytes());
    let crc = format::compute_checksum(
        &buf_bytes[h1_start..h1_start + format::HEADER_SIZE as usize],
        4, // checksum field offset within Header
    );
    buf_bytes[h1_start + 4..h1_start + 8].copy_from_slice(&crc.to_le_bytes());

    // Header 2 (sequence number 1).
    header.sequence_number = 1;
    header.checksum = 0;
    let h2_start = format::HEADER_OFFSET_2 as usize;
    let h2_end = h2_start + size_of::<Header>();
    buf_bytes[h2_start..h2_end].copy_from_slice(header.as_bytes());
    let crc = format::compute_checksum(
        &buf_bytes[h2_start..h2_start + format::HEADER_SIZE as usize],
        4,
    );
    buf_bytes[h2_start + 4..h2_start + 8].copy_from_slice(&crc.to_le_bytes());

    // Region table 1.
    let rt_start = format::REGION_TABLE_OFFSET as usize;
    let mut rt_header = RegionTableHeader::new_zeroed();
    rt_header.signature = format::REGION_TABLE_SIGNATURE;
    rt_header.entry_count = 2;

    let rt_header_bytes = rt_header.as_bytes();
    buf_bytes[rt_start..rt_start + rt_header_bytes.len()].copy_from_slice(rt_header_bytes);

    // BAT region entry.
    let entry_offset = rt_start + size_of::<RegionTableHeader>();
    let bat_entry = RegionTableEntry {
        guid: format::BAT_REGION_GUID,
        file_offset: bat_offset,
        length: bat_length as u32,
        flags: RegionTableEntryFlags::new().with_required(true),
    };
    let bat_entry_bytes = bat_entry.as_bytes();
    buf_bytes[entry_offset..entry_offset + bat_entry_bytes.len()].copy_from_slice(bat_entry_bytes);

    // Metadata region entry.
    let entry_offset2 = entry_offset + size_of::<RegionTableEntry>();
    let meta_entry = RegionTableEntry {
        guid: format::METADATA_REGION_GUID,
        file_offset: metadata_offset,
        length: metadata_length as u32,
        flags: RegionTableEntryFlags::new().with_required(true),
    };
    let meta_entry_bytes = meta_entry.as_bytes();
    buf_bytes[entry_offset2..entry_offset2 + meta_entry_bytes.len()]
        .copy_from_slice(meta_entry_bytes);

    // Compute region table checksum over the full 64 KiB region.
    let rt_end = rt_start + format::REGION_TABLE_SIZE as usize;
    let crc = format::compute_checksum(&buf_bytes[rt_start..rt_end], 4);
    buf_bytes[rt_start + 4..rt_start + 8].copy_from_slice(&crc.to_le_bytes());

    // Copy region table 1 to region table 2.
    let alt_start = format::ALT_REGION_TABLE_OFFSET as usize;
    buf_bytes.copy_within(rt_start..rt_end, alt_start);

    // Write the header area.
    file.write_from(0, buf).await.map_err(CreateError::Write)?;

    // --- Zero the log region ---

    file.zero_range(log_offset, log_length)
        .await
        .map_err(CreateError::Write)?;

    // --- Build and write the metadata table ---

    let mut meta_buf = file.alloc_buffer(metadata_length as usize);
    let meta_bytes = meta_buf.as_mut();

    let mut table_header = MetadataTableHeader::new_zeroed();
    table_header.signature = format::METADATA_TABLE_SIGNATURE;

    let mut entry_count: u16 = 0;
    let entries_start = size_of::<MetadataTableHeader>();
    let mut entry_write_offset = entries_start;
    let mut item_data_offset = format::METADATA_TABLE_SIZE as u32;

    // Helper: write a metadata table entry.
    let add_entry = |buf: &mut [u8],
                     entry_write_offset: &mut usize,
                     entry_count: &mut u16,
                     item_id: Guid,
                     offset: u32,
                     length: u32,
                     is_required: bool,
                     is_virtual_disk: bool| {
        let entry = MetadataTableEntry {
            item_id,
            offset,
            length,
            flags: MetadataTableEntryFlags::new()
                .with_is_required(is_required)
                .with_is_virtual_disk(is_virtual_disk),
            reserved2: 0,
        };
        let bytes = entry.as_bytes();
        buf[*entry_write_offset..*entry_write_offset + bytes.len()].copy_from_slice(bytes);
        *entry_write_offset += bytes.len();
        *entry_count += 1;
    };

    // 1. File parameters (IsRequired only).
    let file_params_len = size_of::<FileParameters>() as u32;
    add_entry(
        meta_bytes,
        &mut entry_write_offset,
        &mut entry_count,
        format::FILE_PARAMETERS_ITEM_GUID,
        item_data_offset,
        file_params_len,
        true,
        false,
    );
    let fp_data_offset = item_data_offset;
    item_data_offset += file_params_len;

    // 2. Virtual disk size (IsRequired + IsVirtualDisk).
    let disk_size_len = 8u32; // u64
    add_entry(
        meta_bytes,
        &mut entry_write_offset,
        &mut entry_count,
        format::VIRTUAL_DISK_SIZE_ITEM_GUID,
        item_data_offset,
        disk_size_len,
        true,
        true,
    );
    let ds_data_offset = item_data_offset;
    item_data_offset += disk_size_len;

    // 3. Logical sector size (IsRequired + IsVirtualDisk).
    let sector_len = 4u32; // u32
    add_entry(
        meta_bytes,
        &mut entry_write_offset,
        &mut entry_count,
        format::LOGICAL_SECTOR_SIZE_ITEM_GUID,
        item_data_offset,
        sector_len,
        true,
        true,
    );
    let lss_data_offset = item_data_offset;
    item_data_offset += sector_len;

    // 4. Physical sector size (IsRequired + IsVirtualDisk).
    add_entry(
        meta_bytes,
        &mut entry_write_offset,
        &mut entry_count,
        format::PHYSICAL_SECTOR_SIZE_ITEM_GUID,
        item_data_offset,
        sector_len,
        true,
        true,
    );
    let pss_data_offset = item_data_offset;
    item_data_offset += sector_len;

    // 5. Page 83 data (IsRequired + IsVirtualDisk).
    let guid_len = 16u32;
    add_entry(
        meta_bytes,
        &mut entry_write_offset,
        &mut entry_count,
        format::PAGE_83_ITEM_GUID,
        item_data_offset,
        guid_len,
        true,
        true,
    );
    let p83_data_offset = item_data_offset;
    item_data_offset += guid_len;

    // 6. Incomplete file (optional, IsRequired only).
    if params.create_incomplete {
        add_entry(
            meta_bytes,
            &mut entry_write_offset,
            &mut entry_count,
            format::INCOMPLETE_FILE_ITEM_GUID,
            0,
            0,
            true,
            false,
        );
    }

    // Verify initial metadata items fit within a single hosting sector.
    debug_assert!(
        (item_data_offset as u64 - format::METADATA_TABLE_SIZE) <= format::MAX_HOSTING_SECTOR_SIZE
    );

    // Write the metadata table header.
    table_header.entry_count = entry_count;
    let th_bytes = table_header.as_bytes();
    meta_bytes[..th_bytes.len()].copy_from_slice(th_bytes);

    // Write the file parameters item data.
    let fp = FileParameters {
        block_size: params.block_size,
        flags: FileParametersFlags::new()
            .with_has_parent(params.has_parent)
            .with_leave_blocks_allocated(params.is_fully_allocated),
    };
    let fp_bytes = fp.as_bytes();
    let fp_off = fp_data_offset as usize;
    meta_bytes[fp_off..fp_off + fp_bytes.len()].copy_from_slice(fp_bytes);

    // Write the virtual disk size item data.
    let ds_off = ds_data_offset as usize;
    meta_bytes[ds_off..ds_off + 8].copy_from_slice(&params.disk_size.to_le_bytes());

    // Write the logical sector size item data.
    let lss_off = lss_data_offset as usize;
    meta_bytes[lss_off..lss_off + 4].copy_from_slice(&params.logical_sector_size.to_le_bytes());

    // Write the physical sector size item data.
    let pss_off = pss_data_offset as usize;
    meta_bytes[pss_off..pss_off + 4].copy_from_slice(&params.physical_sector_size.to_le_bytes());

    // Write the page 83 item data.
    let p83_off = p83_data_offset as usize;
    meta_bytes[p83_off..p83_off + 16].copy_from_slice(params.page_83_data.as_bytes());

    // Write the metadata region.
    file.write_from(metadata_offset, meta_buf)
        .await
        .map_err(CreateError::Write)?;

    // --- Zero the BAT region ---

    file.zero_range(bat_offset, bat_length)
        .await
        .map_err(CreateError::Write)?;

    // --- Set file size ---

    let mut file_size = bat_offset + bat_length;

    // Apply block alignment padding if requested.
    if params.block_alignment as u64 > format::REGION_ALIGNMENT
        && params.block_alignment <= params.block_size
    {
        file_size = round_up(file_size, params.block_alignment as u64);
    }

    file.set_file_size(file_size)
        .await
        .map_err(CreateError::Write)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format;
    use crate::tests::support::InMemoryFile;
    use pal_async::async_test;
    use zerocopy::FromBytes;

    /// Read a little-endian u64 from a byte slice at the given offset.
    fn read_u64(data: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
    }

    /// Read a little-endian u32 from a byte slice at the given offset.
    fn read_u32(data: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
    }

    /// Read a Header from the snapshot at the given offset.
    fn read_header(snapshot: &[u8], offset: usize) -> Header {
        Header::read_from_bytes(&snapshot[offset..offset + size_of::<Header>()])
            .unwrap()
            .clone()
    }

    /// Read the region table header from the snapshot at the given offset.
    fn read_region_table_header(snapshot: &[u8], offset: usize) -> RegionTableHeader {
        RegionTableHeader::read_from_bytes(
            &snapshot[offset..offset + size_of::<RegionTableHeader>()],
        )
        .unwrap()
        .clone()
    }

    /// Read metadata table header from the metadata region.
    fn read_metadata_table_header(snapshot: &[u8], meta_offset: usize) -> MetadataTableHeader {
        MetadataTableHeader::read_from_bytes(
            &snapshot[meta_offset..meta_offset + size_of::<MetadataTableHeader>()],
        )
        .unwrap()
        .clone()
    }

    /// Read a metadata table entry at the given index (0-based).
    fn read_metadata_entry(
        snapshot: &[u8],
        meta_offset: usize,
        index: usize,
    ) -> MetadataTableEntry {
        let entry_offset = meta_offset
            + size_of::<MetadataTableHeader>()
            + index * size_of::<MetadataTableEntry>();
        MetadataTableEntry::read_from_bytes(
            &snapshot[entry_offset..entry_offset + size_of::<MetadataTableEntry>()],
        )
        .unwrap()
        .clone()
    }

    #[async_test]
    async fn create_default_params() {
        let disk_size = format::GB1;
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size,
            ..Default::default()
        };
        create(&file, &mut params).await.unwrap();

        assert_eq!(params.disk_size, disk_size);
        assert_eq!(params.block_size, format::DEFAULT_BLOCK_SIZE);
        assert_eq!(params.logical_sector_size, 512);
        assert_eq!(params.physical_sector_size, 512);

        let snapshot = file.snapshot();
        let file_size = file.file_size().await.unwrap();

        // File identifier signature at offset 0.
        let sig = read_u64(&snapshot, 0);
        assert_eq!(sig, format::FILE_IDENTIFIER_SIGNATURE);

        // Header 1 at 64K.
        let h1 = read_header(&snapshot, format::HEADER_OFFSET_1 as usize);
        assert_eq!(h1.signature, format::HEADER_SIGNATURE);
        assert!(format::validate_checksum(
            &snapshot[format::HEADER_OFFSET_1 as usize
                ..format::HEADER_OFFSET_1 as usize + format::HEADER_SIZE as usize],
            4
        ));

        // Header 2 at 128K.
        let h2 = read_header(&snapshot, format::HEADER_OFFSET_2 as usize);
        assert_eq!(h2.signature, format::HEADER_SIGNATURE);
        assert!(format::validate_checksum(
            &snapshot[format::HEADER_OFFSET_2 as usize
                ..format::HEADER_OFFSET_2 as usize + format::HEADER_SIZE as usize],
            4
        ));

        // Region table 1 at 192K.
        let rt = read_region_table_header(&snapshot, format::REGION_TABLE_OFFSET as usize);
        assert_eq!(rt.signature, format::REGION_TABLE_SIGNATURE);
        assert_eq!(rt.entry_count, 2);
        assert!(format::validate_checksum(
            &snapshot[format::REGION_TABLE_OFFSET as usize
                ..format::REGION_TABLE_OFFSET as usize + format::REGION_TABLE_SIZE as usize],
            4
        ));

        // Region table 2 checksum.
        assert!(format::validate_checksum(
            &snapshot[format::ALT_REGION_TABLE_OFFSET as usize
                ..format::ALT_REGION_TABLE_OFFSET as usize + format::REGION_TABLE_SIZE as usize],
            4
        ));

        // Metadata region starts at 2 MiB.
        let meta_offset = 2 * format::MB1 as usize;
        let mth = read_metadata_table_header(&snapshot, meta_offset);
        assert_eq!(mth.signature, format::METADATA_TABLE_SIGNATURE);
        assert_eq!(mth.entry_count, 5);

        // BAT region should be all zeros.
        let bat_offset = 3 * format::MB1 as usize;
        // Compute expected BAT length.
        let data_block_count = ceil_div(disk_size, format::DEFAULT_BLOCK_SIZE as u64);
        let chunk_ratio = chunk_block_count(format::DEFAULT_BLOCK_SIZE, 512);
        let bat_entry_count = data_block_count + data_block_count / chunk_ratio as u64;
        let bat_sec_count = ceil_div(bat_entry_count, format::ENTRIES_PER_BAT_PAGE);
        let bat_len = round_up(
            bat_sec_count * format::CACHE_PAGE_SIZE,
            format::REGION_ALIGNMENT,
        ) as usize;
        assert!(
            snapshot[bat_offset..bat_offset + bat_len]
                .iter()
                .all(|&b| b == 0)
        );

        // File size should cover all regions.
        assert_eq!(file_size, (bat_offset + bat_len) as u64);
    }

    #[async_test]
    async fn create_validates_disk_size_zero() {
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size: 0,
            ..Default::default()
        };
        assert!(create(&file, &mut params).await.is_err());
    }

    #[async_test]
    async fn create_validates_disk_size_alignment() {
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size: 1000, // not a multiple of 512
            logical_sector_size: 512,
            ..Default::default()
        };
        assert!(create(&file, &mut params).await.is_err());
    }

    #[async_test]
    async fn create_validates_sector_sizes() {
        let file = InMemoryFile::new(0);

        // Invalid logical sector size.
        let mut params = CreateParams {
            disk_size: format::GB1,
            logical_sector_size: 1024,
            ..Default::default()
        };
        assert!(create(&file, &mut params).await.is_err());

        // Invalid physical sector size.
        let mut params = CreateParams {
            disk_size: format::GB1,
            physical_sector_size: 8192,
            ..Default::default()
        };
        assert!(create(&file, &mut params).await.is_err());
    }

    #[async_test]
    async fn create_validates_block_size() {
        let file = InMemoryFile::new(0);

        // Not a multiple of 1 MiB.
        let mut params = CreateParams {
            disk_size: format::GB1,
            block_size: 500_000,
            ..Default::default()
        };
        assert!(create(&file, &mut params).await.is_err());

        // Greater than maximum (256 MiB).
        let mut params = CreateParams {
            disk_size: format::GB1,
            block_size: 512 * 1024 * 1024,
            ..Default::default()
        };
        assert!(create(&file, &mut params).await.is_err());
    }

    #[async_test]
    async fn create_validates_block_alignment() {
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size: format::GB1,
            block_alignment: 3, // not a power of 2
            ..Default::default()
        };
        assert!(create(&file, &mut params).await.is_err());
    }

    #[async_test]
    async fn create_with_512_sectors() {
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size: format::GB1,
            logical_sector_size: 512,
            physical_sector_size: 512,
            ..Default::default()
        };
        create(&file, &mut params).await.unwrap();
        assert_eq!(params.logical_sector_size, 512);
        assert_eq!(params.physical_sector_size, 512);

        let snapshot = file.snapshot();
        let meta_offset = 2 * format::MB1 as usize;

        // Find the logical sector size entry (index 2) and physical (index 3).
        let lss_entry = read_metadata_entry(&snapshot, meta_offset, 2);
        assert_eq!(lss_entry.item_id, format::LOGICAL_SECTOR_SIZE_ITEM_GUID);
        let lss_val = read_u32(&snapshot, meta_offset + lss_entry.offset as usize);
        assert_eq!(lss_val, 512);

        let pss_entry = read_metadata_entry(&snapshot, meta_offset, 3);
        assert_eq!(pss_entry.item_id, format::PHYSICAL_SECTOR_SIZE_ITEM_GUID);
        let pss_val = read_u32(&snapshot, meta_offset + pss_entry.offset as usize);
        assert_eq!(pss_val, 512);
    }

    #[async_test]
    async fn create_with_4k_sectors() {
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size: format::GB1,
            logical_sector_size: 4096,
            physical_sector_size: 4096,
            ..Default::default()
        };
        create(&file, &mut params).await.unwrap();
        assert_eq!(params.logical_sector_size, 4096);
        assert_eq!(params.physical_sector_size, 4096);

        let snapshot = file.snapshot();
        let meta_offset = 2 * format::MB1 as usize;

        let lss_entry = read_metadata_entry(&snapshot, meta_offset, 2);
        let lss_val = read_u32(&snapshot, meta_offset + lss_entry.offset as usize);
        assert_eq!(lss_val, 4096);

        let pss_entry = read_metadata_entry(&snapshot, meta_offset, 3);
        let pss_val = read_u32(&snapshot, meta_offset + pss_entry.offset as usize);
        assert_eq!(pss_val, 4096);
    }

    #[async_test]
    async fn create_various_block_sizes() {
        let block_sizes: Vec<u32> = vec![1, 2, 4, 8, 16, 32, 64, 128, 256]
            .into_iter()
            .map(|m| m * format::MB1 as u32)
            .collect();

        for &bs in &block_sizes {
            let file = InMemoryFile::new(0);
            let mut params = CreateParams {
                disk_size: format::GB1,
                block_size: bs,
                ..Default::default()
            };
            let result = create(&file, &mut params).await;
            assert!(result.is_ok(), "failed for block_size={bs}");

            let snapshot = file.snapshot();
            let sig = read_u64(&snapshot, 0);
            assert_eq!(sig, format::FILE_IDENTIFIER_SIGNATURE);
        }
    }

    #[async_test]
    async fn create_block_alignment() {
        // No alignment: file ends right after BAT.
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size: format::GB1,
            block_alignment: 0,
            ..Default::default()
        };
        create(&file, &mut params).await.unwrap();
        let size_no_align = file.file_size().await.unwrap();

        // With 2 MiB alignment.
        let file2 = InMemoryFile::new(0);
        let align = 2 * format::MB1 as u32;
        let mut params2 = CreateParams {
            disk_size: format::GB1,
            block_alignment: align,
            ..Default::default()
        };
        create(&file2, &mut params2).await.unwrap();
        let size_aligned = file2.file_size().await.unwrap();

        // Aligned size should be >= non-aligned and a multiple of alignment.
        assert!(size_aligned >= size_no_align);
        assert_eq!(size_aligned % align as u64, 0);

        // With alignment == block_size (should be honored since
        // block_alignment <= block_size).
        let file3 = InMemoryFile::new(0);
        let mut params3 = CreateParams {
            disk_size: format::GB1,
            block_alignment: params.block_size,
            ..Default::default()
        };
        create(&file3, &mut params3).await.unwrap();
        let size3 = file3.file_size().await.unwrap();
        assert_eq!(size3 % params.block_size as u64, 0);
    }

    #[async_test]
    async fn create_differencing_disk() {
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size: format::GB1,
            has_parent: true,
            ..Default::default()
        };
        create(&file, &mut params).await.unwrap();

        let snapshot = file.snapshot();
        let meta_offset = 2 * format::MB1 as usize;

        // File parameters entry (index 0).
        let fp_entry = read_metadata_entry(&snapshot, meta_offset, 0);
        assert_eq!(fp_entry.item_id, format::FILE_PARAMETERS_ITEM_GUID);

        // Read the FileParameters data.
        let fp_off = meta_offset + fp_entry.offset as usize;
        let fp = FileParameters::read_from_bytes(
            &snapshot[fp_off..fp_off + size_of::<FileParameters>()],
        )
        .unwrap();
        assert!(fp.flags.has_parent());

        // BAT entry count should include sector bitmap entries.
        let data_block_count = ceil_div(format::GB1, format::DEFAULT_BLOCK_SIZE as u64);
        let chunk_ratio = chunk_block_count(format::DEFAULT_BLOCK_SIZE, 512);
        let sbm_count = ceil_div(data_block_count, chunk_ratio as u64);
        let bat_entry_count_diff = sbm_count * (chunk_ratio as u64 + 1);
        let bat_entry_count_nondiff = data_block_count + data_block_count / chunk_ratio as u64;
        // Differencing should have more entries.
        assert!(bat_entry_count_diff > bat_entry_count_nondiff);
    }

    #[async_test]
    async fn create_incomplete() {
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size: format::GB1,
            create_incomplete: true,
            ..Default::default()
        };
        create(&file, &mut params).await.unwrap();

        let snapshot = file.snapshot();
        let meta_offset = 2 * format::MB1 as usize;
        let mth = read_metadata_table_header(&snapshot, meta_offset);
        assert_eq!(mth.entry_count, 6);

        // The 6th entry (index 5) should be the incomplete file item.
        let entry = read_metadata_entry(&snapshot, meta_offset, 5);
        assert_eq!(entry.item_id, format::INCOMPLETE_FILE_ITEM_GUID);
        assert!(entry.flags.is_required());
        assert!(!entry.flags.is_virtual_disk());
        assert_eq!(entry.offset, 0);
        assert_eq!(entry.length, 0);
    }

    #[async_test]
    async fn create_headers_have_different_sequence_numbers() {
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size: format::GB1,
            ..Default::default()
        };
        create(&file, &mut params).await.unwrap();

        let snapshot = file.snapshot();
        let h1 = read_header(&snapshot, format::HEADER_OFFSET_1 as usize);
        let h2 = read_header(&snapshot, format::HEADER_OFFSET_2 as usize);

        assert_eq!(h1.sequence_number, 0);
        assert_eq!(h2.sequence_number, 1);
    }

    #[async_test]
    async fn create_region_tables_are_identical() {
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size: format::GB1,
            ..Default::default()
        };
        create(&file, &mut params).await.unwrap();

        let snapshot = file.snapshot();
        let rt1_start = format::REGION_TABLE_OFFSET as usize;
        let rt1_end = rt1_start + format::REGION_TABLE_SIZE as usize;
        let rt2_start = format::ALT_REGION_TABLE_OFFSET as usize;
        let rt2_end = rt2_start + format::REGION_TABLE_SIZE as usize;

        assert_eq!(&snapshot[rt1_start..rt1_end], &snapshot[rt2_start..rt2_end]);
    }
}
