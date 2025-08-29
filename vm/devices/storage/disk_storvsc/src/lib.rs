// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Disk backend implementation that uses a user-mode storvsc driver.

#![forbid(unsafe_code)]

use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::UnmapBehavior;
use igvm_defs::PAGE_SIZE_4K;
use inspect::Inspect;
use scsi_defs::ScsiOp;
use scsi_defs::ScsiStatus;
use std::sync::Arc;
use storvsc_driver::StorvscDriver;
use storvsc_driver::StorvscErrorKind;
use vmbus_user_channel::MappedRingMem;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// Maximum number of retries when a retryable failure occurs, which should really only happen
/// during servicing. Servicing shouldn't happen frequently enough to make more than one retry
/// necessary, but provide some buffer room.
const MAX_RETRIES: usize = 5;

/// Disk backend using a storvsc driver to the host.
#[derive(Inspect)]
pub struct StorvscDisk {
    #[inspect(skip)]
    driver: Arc<StorvscDriver<MappedRingMem>>,
    lun: u8,
    num_sectors: u64,
    sector_size: u32,
    disk_id: Option<[u8; 16]>,
    optimal_unmap_sectors: u32,
    read_only: bool,
}

impl StorvscDisk {
    /// Creates a new storvsc-backed disk that uses the provided storvsc driver.
    pub fn new(driver: Arc<StorvscDriver<MappedRingMem>>, lun: u8) -> Self {
        let mut disk = Self {
            driver,
            lun,
            num_sectors: 0,
            sector_size: 0,
            disk_id: None,
            optimal_unmap_sectors: 0,
            read_only: false,
        };
        disk.scan_metadata();
        disk
    }
}

impl StorvscDisk {
    fn scan_metadata(&mut self) {
        // Allocate region for data in for READ_CAPACITY(16), MODE_SENSE(10), and INQUIRY (Block Limits VPD)
        // TODO: When we can allocate continguous pages, switch to that instead of using a single page and assert.
        assert!(
            size_of::<scsi_defs::ReadCapacity16Data>()
                .max(size_of::<scsi_defs::ModeParameterHeader10>())
                .max(size_of::<scsi_defs::VpdBlockLimitsDescriptor>()) as u64
                <= PAGE_SIZE_4K
        );
        let data_in_size = PAGE_SIZE_4K as usize;
        let data_in = match self.driver.allocate_dma_buffer(data_in_size) {
            Ok(buf) => buf,
            Err(err) => {
                tracing::error!(
                    error = err.to_string(),
                    "Unable to allocate DMA buffer to read disk metadata"
                );
                return;
            }
        };

        // READ_CAPACITY(16) returns number of sectors and sector size in bytes.
        let read_capacity16_cdb = scsi_defs::ServiceActionIn16 {
            operation_code: ScsiOp::READ_CAPACITY16,
            service_action: scsi_defs::SERVICE_ACTION_READ_CAPACITY16,
            allocation_length: (data_in_size as u32).into(),
            ..FromZeros::new_zeroed()
        };
        match futures::executor::block_on(self.send_scsi_request(
            read_capacity16_cdb.as_bytes(),
            read_capacity16_cdb.operation_code,
            data_in.pfns()[0] * PAGE_SIZE_4K,
            data_in_size,
            true,
        )) {
            Ok(resp) => {
                match resp.scsi_status {
                    ScsiStatus::GOOD => {
                        let capacity = data_in.read_obj::<scsi_defs::ReadCapacity16Data>(0);
                        self.num_sectors = capacity.ex.logical_block_address.into();
                        self.num_sectors += 1; // Add one to include the last sector
                        self.sector_size = capacity.ex.bytes_per_block.into();
                    }
                    _ => {
                        tracing::error!(
                            scsi_status = ?resp.scsi_status,
                            srb_status = ?resp.srb_status,
                            "READ_CAPACITY16 failed"
                        );
                        return;
                    }
                }
            }
            Err(err) => {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "READ_CAPACITY16 failed"
                );
            }
        }

        // MODE_SENSE10 to get whether read-only. This is in the header, so it doesn't matter which page we request.
        let mode_sense10_cdb = scsi_defs::ModeSense10 {
            operation_code: ScsiOp::MODE_SENSE10,
            flags2: scsi_defs::ModeSenseFlags::new().with_page_code(scsi_defs::MODE_PAGE_ALL),
            sub_page_code: 0,
            allocation_length: (data_in_size as u16).into(),
            ..FromZeros::new_zeroed()
        };

        match futures::executor::block_on(self.send_scsi_request(
            mode_sense10_cdb.as_bytes(),
            mode_sense10_cdb.operation_code,
            data_in.pfns()[0] * PAGE_SIZE_4K,
            data_in_size,
            true,
        )) {
            Ok(resp) => match resp.scsi_status {
                ScsiStatus::GOOD => {
                    let mode_header = data_in.read_obj::<scsi_defs::ModeParameterHeader10>(0);
                    self.read_only = mode_header.device_specific_parameter
                        & scsi_defs::MODE_DSP_WRITE_PROTECT
                        != 0;
                }
                _ => {
                    tracing::error!(
                        scsi_status = ?resp.scsi_status,
                        srb_status = ?resp.srb_status,
                        "MODE_SENSE10 failed"
                    );
                }
            },
            Err(err) => {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "MODE_SENSE10 failed"
                );
            }
        }

        // INQUIRY for the Supported Pages VPD page to see if Block Limits VPD is supported.
        self.optimal_unmap_sectors = 0;
        let inquiry_supported_pages_cdb = scsi_defs::CdbInquiry {
            operation_code: ScsiOp::INQUIRY,
            flags: scsi_defs::InquiryFlags::new().with_vpd(true),
            page_code: scsi_defs::VPD_SUPPORTED_PAGES,
            allocation_length: (data_in_size as u16).into(),
            ..FromZeros::new_zeroed()
        };

        match futures::executor::block_on(self.send_scsi_request(
            inquiry_supported_pages_cdb.as_bytes(),
            inquiry_supported_pages_cdb.operation_code,
            data_in.pfns()[0] * PAGE_SIZE_4K,
            data_in_size,
            true,
        )) {
            Ok(resp) => match resp.scsi_status {
                ScsiStatus::GOOD => {
                    let mut buf_pos = 0;
                    let vpd_header = data_in.read_obj::<scsi_defs::VpdPageHeader>(0);
                    buf_pos += size_of::<scsi_defs::VpdPageHeader>();
                    while buf_pos
                        < vpd_header.page_length as usize + size_of::<scsi_defs::VpdPageHeader>()
                    {
                        if data_in.read_obj::<u8>(buf_pos) == scsi_defs::VPD_BLOCK_LIMITS {
                            // INQUIRY for the Block Limits VPD page returns the optimal unmap sectors.
                            let inquiry_block_limits_cdb = scsi_defs::CdbInquiry {
                                operation_code: ScsiOp::INQUIRY,
                                flags: scsi_defs::InquiryFlags::new().with_vpd(true),
                                page_code: scsi_defs::VPD_BLOCK_LIMITS,
                                allocation_length: (data_in_size as u16).into(),
                                ..FromZeros::new_zeroed()
                            };

                            match futures::executor::block_on(self.send_scsi_request(
                                inquiry_block_limits_cdb.as_bytes(),
                                inquiry_block_limits_cdb.operation_code,
                                data_in.pfns()[0] * PAGE_SIZE_4K,
                                data_in_size,
                                true,
                            )) {
                                Ok(resp) => match resp.scsi_status {
                                    ScsiStatus::GOOD => {
                                        let block_limits_vpd =
                                            data_in
                                                .read_obj::<scsi_defs::VpdBlockLimitsDescriptor>(0);
                                        self.optimal_unmap_sectors =
                                            block_limits_vpd.optimal_unmap_granularity.into();
                                    }
                                    _ => {
                                        tracing::error!(
                                            scsi_status = ?resp.scsi_status,
                                            srb_status = ?resp.srb_status,
                                            "INQUIRY for Block Limits VPD failed"
                                        );
                                    }
                                },
                                Err(err) => {
                                    tracing::error!(
                                        error = &err as &dyn std::error::Error,
                                        "INQUIRY for Block Limits VPD failed"
                                    );
                                }
                            }
                        }
                        buf_pos += 1;
                    }
                }
                _ => {
                    tracing::error!(
                        scsi_status = ?resp.scsi_status,
                        srb_status = ?resp.srb_status,
                        "INQUIRY for Supported Pages VPD failed"
                    );
                }
            },
            Err(err) => {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "INQUIRY for Supported Pages VPD failed"
                );
            }
        }

        // INQUIRY for the Device Identification VPD page returns the designator (disk ID).
        self.disk_id = None;
        let inquiry_device_identification_cdb = scsi_defs::CdbInquiry {
            operation_code: ScsiOp::INQUIRY,
            flags: scsi_defs::InquiryFlags::new().with_vpd(true),
            page_code: scsi_defs::VPD_DEVICE_IDENTIFIERS,
            allocation_length: (data_in_size as u16).into(),
            ..FromZeros::new_zeroed()
        };

        match futures::executor::block_on(self.send_scsi_request(
            inquiry_device_identification_cdb.as_bytes(),
            inquiry_device_identification_cdb.operation_code,
            data_in.pfns()[0] * PAGE_SIZE_4K,
            data_in_size,
            true,
        )) {
            Ok(resp) => match resp.scsi_status {
                ScsiStatus::GOOD => {
                    let mut buf_pos = 0;
                    let vpd_header = data_in.read_obj::<scsi_defs::VpdPageHeader>(0);
                    buf_pos += size_of::<scsi_defs::VpdPageHeader>();
                    while buf_pos < vpd_header.page_length as usize + 4 {
                        let designator_header =
                            data_in.read_obj::<scsi_defs::VpdIdentificationDescriptor>(buf_pos);
                        buf_pos += size_of::<scsi_defs::VpdIdentificationDescriptor>();
                        match designator_header.identifiertype {
                            scsi_defs::VPD_IDENTIFIER_TYPE_FCPH_NAME => {
                                // Reinterpret as NAA ID designator.
                                let designator_naa =
                                    data_in.read_obj::<scsi_defs::VpdNaaId>(buf_pos);
                                let mut disk_id = [0u8; 16];
                                disk_id[0] = designator_naa.ouid_msb;
                                disk_id[1..3]
                                    .copy_from_slice(designator_naa.ouid_middle.as_slice());
                                disk_id[3] = designator_naa.ouid_lsb;
                                disk_id[4..]
                                    .copy_from_slice(designator_naa.vendor_specific_id.as_slice());
                                self.disk_id = Some(disk_id);
                                break;
                            }
                            _ => {
                                buf_pos += size_of::<scsi_defs::VpdIdentificationDescriptor>()
                                    + designator_header.identifier_length as usize;
                            }
                        }
                    }
                }
                _ => {
                    tracing::error!(
                        scsi_status = ?resp.scsi_status,
                        srb_status = ?resp.srb_status,
                        "INQUIRY for Device Identification VPD failed"
                    );
                }
            },
            Err(err) => {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "INQUIRY for Block Limits VPD failed"
                );
            }
        }

        tracing::info!(
            num_sectors = self.num_sectors,
            sector_size = self.sector_size,
            read_only = self.read_only,
            optimal_unmap_sectors = self.optimal_unmap_sectors,
            disk_id = ?self.disk_id,
            "Read storvsc disk metadata"
        );
    }

    fn generate_scsi_request(
        &self,
        data_transfer_length: u32,
        payload: &[u8],
        is_read: bool,
    ) -> storvsp_protocol::ScsiRequest {
        assert!(payload.len() <= storvsp_protocol::MAX_DATA_BUFFER_LENGTH_WITH_PADDING);
        let data_in: u8 = if is_read { 1 } else { 0 };
        let mut request = storvsp_protocol::ScsiRequest {
            target_id: 0,
            path_id: 0,
            lun: self.lun,
            length: storvsp_protocol::SCSI_REQUEST_LEN_V2 as u16,
            cdb_length: payload.len() as u8,
            data_transfer_length,
            data_in,
            ..FromZeros::new_zeroed()
        };
        request.payload[0..payload.len()].copy_from_slice(payload);
        request
    }

    async fn send_scsi_request(
        &self,
        cdb: &[u8],
        op: ScsiOp,
        buf_gpa: u64,
        byte_len: usize,
        is_read: bool,
    ) -> Result<storvsp_protocol::ScsiRequest, DiskError> {
        let request = self.generate_scsi_request(byte_len as u32, cdb, is_read);

        let mut num_tries = 0;
        loop {
            match self.driver.send_request(&request, buf_gpa, byte_len).await {
                Ok(resp) => match resp.scsi_status {
                    ScsiStatus::GOOD => break Ok(resp), // Request succeeded, break out of loop
                    _ => {
                        tracing::error!(?op, scsi_status = ?resp.scsi_status, "SCSI request failed");
                        Err(DiskError::Io(std::io::Error::other(format!(
                            "SCSI request failed, op={:?}, scsi_status={:?}, srb_status={:?}",
                            op, resp.scsi_status, resp.srb_status
                        ))))
                    }
                },
                Err(err) => {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "SCSI request failed"
                    );
                    match err.kind() {
                        StorvscErrorKind::CompletionError => Err(DiskError::Io(
                            std::io::Error::new(std::io::ErrorKind::Interrupted, err),
                        )),
                        StorvscErrorKind::Cancelled => Err(DiskError::Io(std::io::Error::new(
                            std::io::ErrorKind::Interrupted,
                            err,
                        ))),
                        StorvscErrorKind::CancelledRetry => {
                            if num_tries < MAX_RETRIES {
                                Ok(())
                            } else {
                                break Err(DiskError::Io(std::io::Error::new(
                                    std::io::ErrorKind::Interrupted,
                                    err,
                                )));
                            }
                        }
                        _ => Err(DiskError::Io(std::io::Error::other(err))),
                    }
                }
            }?;
            num_tries += 1;
        }
    }
}

impl DiskIo for StorvscDisk {
    fn disk_type(&self) -> &str {
        "storvsc"
    }

    fn sector_count(&self) -> u64 {
        self.num_sectors
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        self.disk_id
    }

    fn physical_sector_size(&self) -> u32 {
        self.sector_size
    }

    fn is_fua_respected(&self) -> bool {
        true
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    async fn read_vectored(
        &self,
        buffers: &scsi_buffers::RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        if self.sector_size == 0 {
            // Disk failed to initialize.
            return Err(DiskError::IllegalBlock);
        }

        if buffers.len() % self.sector_size as usize != 0 {
            // Buffer length must be a multiple of sector size.
            return Err(DiskError::InvalidInput);
        }

        // Get LockedPages for the buffers to pass to the storvsc client.
        let locked_buffers = buffers
            .guest_memory()
            .lock_gpns(false, buffers.range().gpns())
            .map_err(|_| DiskError::ReservationConflict)?;

        let cdb = scsi_defs::Cdb16 {
            operation_code: ScsiOp::READ16,
            logical_block: sector.into(),
            transfer_blocks: (buffers.len() as u32 / self.sector_size).into(),
            ..FromZeros::new_zeroed()
        };

        self.send_scsi_request(
            cdb.as_bytes(),
            cdb.operation_code,
            locked_buffers.va(),
            buffers.len(),
            true,
        )
        .await
        .map(|_| ())
    }

    async fn write_vectored(
        &self,
        buffers: &scsi_buffers::RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        if self.sector_size == 0 {
            // Disk failed to initialize.
            return Err(DiskError::IllegalBlock);
        }

        if buffers.len() % self.sector_size as usize != 0 {
            // Buffer length must be a multiple of sector size.
            return Err(DiskError::InvalidInput);
        }

        // Get LockedPages for the buffers to pass to the storvsc client.
        let locked_buffers = buffers
            .guest_memory()
            .lock_gpns(false, buffers.range().gpns())
            .map_err(|_| DiskError::ReservationConflict)?;

        let cdb = scsi_defs::Cdb16 {
            operation_code: ScsiOp::WRITE16,
            flags: scsi_defs::Cdb16Flags::new().with_fua(fua),
            logical_block: sector.into(),
            transfer_blocks: (buffers.len() as u32 / self.sector_size).into(),
            ..FromZeros::new_zeroed()
        };

        self.send_scsi_request(
            cdb.as_bytes(),
            cdb.operation_code,
            locked_buffers.va(),
            buffers.len(),
            false,
        )
        .await
        .map(|_| ())
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        if self.sector_size == 0 {
            // Disk failed to initialize.
            return Err(DiskError::IllegalBlock);
        }

        let cdb = scsi_defs::Cdb16 {
            operation_code: ScsiOp::SYNCHRONIZE_CACHE16,
            logical_block: 0.into(),
            transfer_blocks: 0.into(), // 0 indicates to sync all sectors
            ..FromZeros::new_zeroed()
        };

        self.send_scsi_request(cdb.as_bytes(), cdb.operation_code, 0, 0, false)
            .await
            .map(|_| ())
    }

    async fn eject(&self) -> Result<(), DiskError> {
        let cdb = scsi_defs::StartStop {
            operation_code: ScsiOp::START_STOP_UNIT,
            flag: scsi_defs::StartStopFlags::new().with_load_eject(true),
            ..FromZeros::new_zeroed()
        };

        self.send_scsi_request(cdb.as_bytes(), cdb.operation_code, 0, 0, false)
            .await
            .map(|_| ())
    }

    async fn unmap(
        &self,
        sector: u64,
        count: u64,
        _block_level_only: bool,
    ) -> Result<(), DiskError> {
        let cdb = scsi_defs::Unmap {
            operation_code: ScsiOp::UNMAP,
            allocation_length: (size_of::<scsi_defs::UnmapBlockDescriptor>() as u16).into(),
            ..FromZeros::new_zeroed()
        };

        let unmap_param_list = scsi_defs::UnmapListHeader {
            data_length: ((size_of::<scsi_defs::UnmapListHeader>() - 2
                + size_of::<scsi_defs::UnmapBlockDescriptor>()) as u16)
                .into(),
            block_descriptor_data_length: (size_of::<scsi_defs::UnmapBlockDescriptor>() as u16)
                .into(),
            ..FromZeros::new_zeroed()
        };

        let unmap_descriptor = scsi_defs::UnmapBlockDescriptor {
            start_lba: sector.into(),
            lba_count: (count as u32).into(), // TODO: what if more than 2^32?
            ..FromZeros::new_zeroed()
        };

        // TODO: When we can allocate continguous pages, switch to that instead of using a single page and assert.
        assert!(
            (size_of::<scsi_defs::UnmapListHeader>() + size_of::<scsi_defs::UnmapBlockDescriptor>())
                as u64
                <= PAGE_SIZE_4K
        );
        let data_out_size = PAGE_SIZE_4K as usize;
        let data_out = match self.driver.allocate_dma_buffer(data_out_size) {
            Ok(buf) => buf,
            Err(err) => {
                tracing::error!(
                    error = err.to_string(),
                    "Unable to allocate DMA buffer for UNMAP"
                );
                return Err(DiskError::Io(std::io::Error::other(err)));
            }
        };
        data_out.write_at(0, unmap_param_list.as_bytes());
        data_out.write_at(
            size_of::<scsi_defs::UnmapListHeader>(),
            unmap_descriptor.as_bytes(),
        );

        self.send_scsi_request(
            cdb.as_bytes(),
            cdb.operation_code,
            data_out.pfns()[0] * PAGE_SIZE_4K,
            data_out_size,
            false,
        )
        .await
        .map(|_| ())
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        UnmapBehavior::Unspecified
    }

    fn optimal_unmap_sectors(&self) -> u32 {
        self.optimal_unmap_sectors
    }

    async fn wait_resize(&self, _sector_count: u64) -> u64 {
        // This is difficult because it cannot update the stored sector count
        todo!()
    }
}
