// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::ScsiError;
use super::SimpleScsiDisk;
use crate::scsi;
use crate::UNMAP_RANGE_DESCRIPTOR_COUNT_MAX;
use guestmem::MemoryRead;
use scsi::AdditionalSenseCode;
use scsi_buffers::RequestBuffers;
use scsi_core::Request;
use zerocopy::FromBytes;

#[derive(Debug, Default)]
struct UnmapInfo {
    descriptor_index: u16,
    total_descriptors: u16,
    start_lba: u64,
    lba_count: u64,
    offset: usize,
}

fn validate_unmap_list_header(
    buffer: &[u8],
    allocation_length: usize,
    unmap_info: &mut UnmapInfo,
) -> Result<(), ScsiError> {
    // Caller already make sure the data buffer is large enough for UNMAP_LIST_HEADER.
    // Now, we can safely access the header.  Validate its content.
    // We will find out how many block descriptors the unmap request has
    let unmap_list_header =
        scsi::UnmapListHeader::read_from_prefix(&buffer[0..size_of::<scsi::UnmapListHeader>()])
            .unwrap()
            .0; // TODO: zerocopy: from-prefix (read_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    let unmap_list_header_length = unmap_list_header.data_length.get() as usize; // TODO: zerocopy: from-prefix (read_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    let expected = allocation_length - size_of_val(&unmap_list_header.data_length);
    if unmap_list_header_length != expected {
        tracelimit::error_ratelimited!(unmap_list_header_length, expected, "validate_unmap_error");
        return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
    }

    // make sure data buffer is large enough for the number of block descriptors
    // the header claims it has
    let block_descriptor_length = unmap_list_header.block_descriptor_data_length.get() as usize;
    let expected = allocation_length - size_of::<scsi::UnmapListHeader>();
    if block_descriptor_length != expected {
        tracelimit::error_ratelimited!(block_descriptor_length, expected, "validate_unmap_error");
        return Err(ScsiError::IllegalRequest(AdditionalSenseCode::NO_SENSE));
    }

    let block_descriptor_count =
        (block_descriptor_length / size_of::<scsi::UnmapBlockDescriptor>()) as u16;

    unmap_info.total_descriptors = block_descriptor_count;
    unmap_info.offset = size_of::<scsi::UnmapListHeader>();

    if block_descriptor_count == 0 {
        return Ok(());
    }

    // limits the number of unmap block descriptors per unmap
    if block_descriptor_count > UNMAP_RANGE_DESCRIPTOR_COUNT_MAX {
        tracelimit::error_ratelimited!(block_descriptor_count, "validate_unmap_error");
        return Err(ScsiError::IllegalRequest(
            AdditionalSenseCode::INVALID_FIELD_PARAMETER_LIST,
        ));
    }

    // At this point, we have an array of UNMAP_BLOCK_DESCRIPTORs.
    // What is in each UNMAP_BLOCK_DESCRIPTOR can still be invalid. That
    // will be validated once we are ready to issue the unmap for that descriptor.
    tracing::trace!(unmap_info = ?unmap_info, "validate_unmap");

    Ok(())
}

pub fn validate_lba_range(sector_count: u64, start_lba: u64, lba_count: u64) -> bool {
    if start_lba >= sector_count {
        tracelimit::error_ratelimited!(start_lba, sector_count, "validate_lba_range_error");
        return false;
    }

    if lba_count > sector_count {
        tracelimit::error_ratelimited!(lba_count, sector_count, "validate_lba_range_error");
        return false;
    }

    if start_lba > sector_count - lba_count {
        tracelimit::error_ratelimited!(
            start_lba,
            lba_count,
            sector_count,
            "validate_lba_range_error"
        );
        false
    } else {
        true
    }
}

impl SimpleScsiDisk {
    fn set_unmap_descriptor(
        &self,
        buffer: &[u8],
        unmap_info: &mut UnmapInfo,
        sector_count: u64,
    ) -> Result<(), ScsiError> {
        let block_descriptor = scsi::UnmapBlockDescriptor::read_from_prefix(
            &buffer[unmap_info.offset..unmap_info.offset + size_of::<scsi::UnmapBlockDescriptor>()],
        )
        .unwrap()
        .0; // TODO: zerocopy: from-prefix (read_from_prefix): use-rest-of-range, err (https://github.com/microsoft/openvmm/issues/759)
        let start_lba = block_descriptor.start_lba.get();
        let lba_count = block_descriptor.lba_count.get() as u64;

        // Validate the descriptor.
        if !validate_lba_range(sector_count, start_lba, lba_count) {
            tracelimit::error_ratelimited!(block_descriptor = ?block_descriptor, "set_unmap_descriptor");
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::ILLEGAL_BLOCK,
            ));
        }

        unmap_info.start_lba = start_lba;
        unmap_info.lba_count = lba_count;

        Ok(())
    }

    fn process_unmap_request(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
    ) -> Result<(Vec<u8>, UnmapInfo), ScsiError> {
        if !self.scsi_parameters.support_unmap {
            return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
        }

        let cdb = scsi::Unmap::read_from_prefix(&request.cdb[..]).unwrap().0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)

        // make sure the data buffer is large enough for UNMAP_LIST_HEADER
        let allocation_length = cdb.allocation_length.get() as usize;
        if allocation_length < size_of::<scsi::UnmapListHeader>()
            || allocation_length != external_data.len()
        {
            tracelimit::error_ratelimited!(allocation_length, "validate_unmap_error");
            return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
        }

        // Read data into buffer for parse
        let mut buffer: Vec<u8> = vec![0; allocation_length];
        external_data
            .reader()
            .read(&mut buffer)
            .map_err(ScsiError::MemoryAccess)?;

        let mut unmap_info = UnmapInfo::default();

        validate_unmap_list_header(&buffer, allocation_length, &mut unmap_info)?;
        Ok((buffer, unmap_info))
    }
}

impl SimpleScsiDisk {
    async fn perform_unmap(
        &self,
        buffer: &[u8],
        unmap_info: &mut UnmapInfo,
        block_level_only: bool,
        sector_count: u64,
    ) -> Result<(), ScsiError> {
        let sector_shift = self.sector_shift;
        let max_lba_count_per_sub_range = (u32::MAX >> sector_shift) as u64;
        let max_lba_count_per_block = (self.sector_size >> sector_shift) as u64; //TODO: get block_size

        loop {
            // Determine if there is more work to do on the current descriptor.
            if unmap_info.lba_count == 0 {
                unmap_info.descriptor_index += 1;
                if unmap_info.descriptor_index >= unmap_info.total_descriptors {
                    return Ok(());
                }

                unmap_info.offset += size_of::<scsi::UnmapBlockDescriptor>();
                self.set_unmap_descriptor(buffer, unmap_info, sector_count)?;

                if unmap_info.lba_count == 0 {
                    continue;
                }
            }

            tracing::debug!(unmap_info = ?unmap_info, "perform_unmap_async");

            // Start working on this descriptor.
            let start_lba = unmap_info.start_lba;
            let mut lba_count = unmap_info.lba_count;

            // Trim the requested Lba count if it exceeds our IO sub-range limit.
            if lba_count > max_lba_count_per_sub_range {
                lba_count = max_lba_count_per_sub_range;
                lba_count -= (start_lba + lba_count) % max_lba_count_per_block;
                tracing::trace!(lba_count, "trim lba_count");
            }

            unmap_info.start_lba += lba_count;
            unmap_info.lba_count -= lba_count;

            tracing::debug!(
                start_lba,
                lba_count,
                start_lba,
                lba_count,
                "dispatching inner unmap"
            );

            if let Err(e) = self
                .disk
                .unmap(start_lba, lba_count, block_level_only)
                .await
            {
                tracing::debug!(error = ?e, "Unmap failures ignored")
            }
        }
    }

    pub(crate) async fn handle_unmap(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
        sector_count: u64,
    ) -> Result<usize, ScsiError> {
        let (buffer, mut unmap_info) = self.process_unmap_request(external_data, request)?;
        if unmap_info.total_descriptors == 0 {
            return Ok(0);
        }

        // We sometimes say we support thin provisioning even if the parser
        // doesn't support it.
        if self.disk.unmap_behavior() == disk_backend::UnmapBehavior::Ignored {
            return Ok(0);
        }

        self.set_unmap_descriptor(&buffer, &mut unmap_info, sector_count)?;

        let block_level_only = request.srb_flags & scsi::SRB_FLAGS_BLOCK_LEVEL_ONLY != 0;
        self.perform_unmap(&buffer, &mut unmap_info, block_level_only, sector_count)
            .await?;

        Ok(0)
    }
}
