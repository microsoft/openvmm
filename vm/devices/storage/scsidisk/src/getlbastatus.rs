// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for the SCSI "Get LBA Status" command.
//!
//! Currently, this command just returns that all blocks are "mapped".

use super::ScsiError;
use super::SimpleScsiDisk;
use disk_backend::Disk;
use disk_backend::DiskError;
use guestmem::MemoryWrite;
use scsi::AdditionalSenseCode;
use scsi_buffers::RequestBuffers;
use scsi_core::Request;
use scsi_defs as scsi;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// Result of a get LBA status request.
#[derive(Debug, Default, Copy, Clone)]
struct DeviceBlockIndexInfo {
    /// The size of the first partial block.
    first_partial_block_size: u32,
    /// The index of the first full block.
    first_full_block_index: u32,
    /// The number of blocks.
    block_count: u32,
    /// The size of the last partial block.
    #[allow(dead_code)]
    last_partial_block_size: u32,
    /// The number of LBAs per block.
    lba_per_block: u64,
}

/// The LBA status of a block.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum LbaStatus {
    /// The block is mapped.
    Mapped,
    /// The block is deallocated.
    #[allow(dead_code)]
    Deallocated,
    /// The block is anchored.
    #[allow(dead_code)]
    Anchored,
}

/// Returns the block index information for the given file offset.
fn file_offset_to_device_block_index_and_length(
    disk: &Disk,
    _start_offset: u64,
    _get_lba_status_range_length: u64,
    _block_size: u64,
) -> DeviceBlockIndexInfo {
    let sector_size = disk.sector_size() as u64;
    let sector_count = disk.sector_count();
    let disk_size = sector_size * sector_count;

    // Treat fully allocation disk or fixed disk as one large block and just return
    // enough descriptors from the LBA requested till the last LBA on disk.
    //
    // LbaPerBlock is a ULONG and technically with MAXULONG * 512 byte sectors,
    // we can get upto 1.99 TB. The LBA descriptor also holds a ULONG
    // LogicalBlockCount and can have an issue for larger than 2TB disks.
    let lba_per_block = std::cmp::min(sector_count, u32::MAX.into());
    let block_size_large = lba_per_block * sector_size;
    let block_count = disk_size.div_ceil(block_size_large) as u32;
    DeviceBlockIndexInfo {
        first_partial_block_size: 0,
        first_full_block_index: 0,
        block_count,
        last_partial_block_size: 0,
        lba_per_block,
    }
}

/// Returns the LBA status for the given block number.
fn get_block_lba_status(
    _block_number: u32,
    _leaf_node_state_only: bool,
) -> Result<LbaStatus, DiskError> {
    Ok(LbaStatus::Mapped)
}

impl SimpleScsiDisk {
    pub(crate) fn handle_get_lba_status(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
        sector_count: u64,
    ) -> Result<usize, ScsiError> {
        let cdb = scsi::GetLbaStatus::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)

        // Validate the request parameters.
        let start_lba = cdb.start_lba.get();
        if start_lba >= sector_count {
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::ILLEGAL_BLOCK,
            ));
        }

        let allocation_length = cdb.allocation_length.get() as usize;
        if allocation_length != external_data.len() {
            return Err(ScsiError::SrbError);
        }

        // A special case in the SCSI spec. Complete the command immediately.
        if allocation_length == 0 {
            return Ok(0);
        }

        // Must meet the minimum response size (a header and one descriptor entry).
        let min = size_of::<scsi::LbaStatusListHeader>() + size_of::<scsi::LbaStatusDescriptor>();
        if allocation_length < min {
            tracelimit::error_ratelimited!(allocation_length, min, "invalid cdb");
            return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
        }

        let sector_size = self.sector_size as u64;
        let disk_size = sector_size * sector_count;
        let block_size = sector_size; // TODO: get block_size from disk

        let start_offset = start_lba << self.sector_shift;
        let get_lba_status_range_length = disk_size - start_offset;
        let total_lba_count_requested = get_lba_status_range_length / sector_size;

        let mut lba_count_remaining = total_lba_count_requested;

        let mut block_index_info = file_offset_to_device_block_index_and_length(
            &self.disk,
            start_offset,
            get_lba_status_range_length,
            block_size,
        );

        let mut block_number = block_index_info.first_full_block_index;
        if block_index_info.first_partial_block_size != 0 {
            block_number -= 1;
            block_index_info.block_count += 1;
        }

        let block_number_max = block_number + block_index_info.block_count;

        // at this point, we have
        //
        // BlockNumber:             First block number to query LBA status (may be partial).
        // FirstPartialBlockSize:   Size of the first block to query LBA status.
        // LastPartialBlockSize:   Size of the last block to query LBA status.
        //
        // BlockCount:              Total number of blocks to query status for, including the first
        //                               and last partial blocks.

        // the output buffer is to look like this
        //
        //  LBA_STATUS_LIST_HEADER
        //  LBA_STATUS_DESCRIPTOR
        //  LBA_STATUS_DESCRIPTOR
        //  ...
        //  LBA_STATUS_DESCRIPTOR
        //
        // Each LBA_STATUS_DESCRIPTOR describes a range of consecutive LBAs having the
        // same LBA status. Since every LBA in a VHD block must have the same LBA status,
        // we can always report all LBAs within the same block using exactly one
        // LBA_STATUS_DESCRIPTOR.  We need as many LBA_STATUS_DESCRIPTORs as
        // BlockCount. We end up using fewer LBA_STATUS_DESCRIPTORs when we
        // realize some neighboring blocks have the same block status and we can group them
        // into the same LBA_STATUS_DESCRIPTOR.

        // Build additional LBA_STATUS_DESCRIPTORs and copy them to the output buffer as
        // long as there is room and we have more block statuses to report.

        let mut lba_descriptors_used = 0;

        // Calculate how many descriptors are available in the buffer which does not exceed
        // the amount expressible by the Parameter Length field.
        // Maximum number of LBA_STATUS_DESCRIPTORs the SCSI-3 spec allows per request.
        const LBA_STATUS_DESCRIPTOR_COUNT_MAX: u32 = (u32::MAX
            - size_of::<scsi::LbaStatusListHeader>() as u32)
            / (size_of::<scsi::LbaStatusDescriptor>() as u32);
        let mut lba_descriptors_available = std::cmp::min(
            LBA_STATUS_DESCRIPTOR_COUNT_MAX,
            ((allocation_length - size_of::<scsi::LbaStatusListHeader>())
                / size_of::<scsi::LbaStatusDescriptor>()) as u32,
        );

        // Get the LBA status for the very first block.
        let leaf_node_state_only =
            request.srb_flags & scsi::SRB_FLAGS_CONSOLIDATEABLE_BLOCKS_ONLY != 0;
        let mut provisioning_status = match get_block_lba_status(block_number, leaf_node_state_only)
        {
            Ok(status) => status,
            Err(e) => return Err(ScsiError::Disk(e)),
        };

        let mut provisioning_status_in_previous_block;
        let mut lba_count_in_current_block;
        let mut descriptor_lba_count;
        let mut buffer: Vec<u8> = vec![0; allocation_length];
        let mut next_lba_status_descriptor = size_of::<scsi::LbaStatusListHeader>();
        let mut next_start_lba = start_lba;
        while lba_descriptors_available != 0 && block_number < block_number_max {
            provisioning_status_in_previous_block = provisioning_status;

            // NextStartLba can be pointing to in the middle of this block.  Thus,
            // the LBA count in this block can be less than a full block.
            lba_count_in_current_block = std::cmp::min(
                block_index_info.lba_per_block - next_start_lba % block_index_info.lba_per_block,
                lba_count_remaining,
            );
            lba_count_remaining -= lba_count_in_current_block;

            // Start a new accumulated LBA count for a new LBA_STATUS_DESCRIPTOR.
            descriptor_lba_count = lba_count_in_current_block;
            block_number += 1;

            // Loop through all subsequent blocks until we see a different status and
            // add it to the total LBA count.
            while block_number < block_number_max {
                // Get the LBA status for the next block.
                // Usually it's a full block except when we get to the last block.
                provisioning_status = match get_block_lba_status(block_number, leaf_node_state_only)
                {
                    Ok(status) => status,
                    Err(e) => return Err(ScsiError::Disk(e)),
                };

                // This block has a different status from what we are looking for.
                // Break out of here so we can composite a new LBA_STATUS_DESCRIPTOR.
                if provisioning_status != provisioning_status_in_previous_block {
                    break;
                }

                lba_count_in_current_block =
                    std::cmp::min(block_index_info.lba_per_block, lba_count_remaining);

                // Only a limited number of LBAs can share the same status descriptor. Finish
                // this descriptor if it is full.
                if descriptor_lba_count + lba_count_in_current_block > u32::MAX.into() {
                    lba_count_remaining -= (u32::MAX as u64) - descriptor_lba_count;
                    descriptor_lba_count = u32::MAX.into();
                    break;
                }

                lba_count_remaining -= lba_count_in_current_block;
                descriptor_lba_count += lba_count_in_current_block;
                block_number += 1;
            }

            let provisioning_status = match provisioning_status_in_previous_block {
                LbaStatus::Mapped => scsi::LBA_STATUS_MAPPED,
                LbaStatus::Deallocated => scsi::LBA_STATUS_DEALLOCATED,
                LbaStatus::Anchored => scsi::LBA_STATUS_ANCHORED,
            };

            // Composite the current LBA_STATUS_DESCRIPTOR and
            // prepare for the next LBA_STATUS_DESCRIPTOR.
            let lba_status_descriptor = scsi::LbaStatusDescriptor {
                start_lba: next_start_lba.into(),
                logical_block_count: (descriptor_lba_count as u32).into(),
                provisioning_status,
                reserved2: [0; 3],
            };

            tracing::trace!(
                lba_status = ?lba_status_descriptor,
                "get_lba_status"
            );

            let new_next_lba_status_descriptor =
                next_lba_status_descriptor + size_of::<scsi::LbaStatusDescriptor>();
            buffer[next_lba_status_descriptor..new_next_lba_status_descriptor]
                .copy_from_slice(lba_status_descriptor.as_bytes());
            lba_descriptors_used += 1;
            lba_descriptors_available -= 1;
            next_start_lba += descriptor_lba_count;
            next_lba_status_descriptor = new_next_lba_status_descriptor;
        }

        // Fill out the header, including the number of contained descriptors.
        let lba_status_descriptors_length =
            lba_descriptors_used * size_of::<scsi::LbaStatusDescriptor>();
        let mut lba_status_list_header = scsi::LbaStatusListHeader::new_zeroed();
        lba_status_list_header.parameter_length = ((lba_status_descriptors_length
            + size_of_val(&lba_status_list_header.reserved))
            as u32)
            .into();

        buffer[0..size_of::<scsi::LbaStatusListHeader>()]
            .copy_from_slice(lba_status_list_header.as_bytes());
        let tx = lba_status_descriptors_length + size_of::<scsi::LbaStatusListHeader>();

        external_data
            .writer()
            .write(&buffer[..tx])
            .map_err(ScsiError::MemoryAccess)?;

        Ok(tx)
    }
}
