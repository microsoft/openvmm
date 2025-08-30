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
use static_assertions::const_assert;
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
    #[inspect(skip)]
    resize_event: Arc<event_listener::Event>,
}

#[derive(Default)]
struct DiskCapacity {
    num_sectors: u64,
    sector_size: u32,
}

impl StorvscDisk {
    /// Creates a new storvsc-backed disk that uses the provided storvsc driver.
    pub fn new(driver: Arc<StorvscDriver<MappedRingMem>>, lun: u8) -> Self {
        let disk = Self {
            driver: driver.clone(),
            lun,
            resize_event: Arc::new(event_listener::Event::new()),
        };
        match driver.add_resize_listener(lun, disk.resize_event.clone()) {
            Ok(()) => {}
            Err(err) => {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "Failed to add resize listener to storvsc driver"
                );
            }
        }
        disk
    }
}

impl StorvscDisk {
    fn disk_capacity(&self) -> DiskCapacity {
        // Allocate region for data in for READ CAPACITY(16)
        // At this time we cannot allocate contiguous pages, but this could be done without an
        // assert if we could guarantee that the allocation is contiguous.
        const_assert!(size_of::<scsi_defs::ReadCapacity16Data>() as u64 <= PAGE_SIZE_4K);
        let data_in_size = PAGE_SIZE_4K as usize;
        let data_in = match self.driver.allocate_dma_buffer(data_in_size) {
            Ok(buf) => buf,
            Err(err) => {
                tracing::error!(
                    error = err.to_string(),
                    "Unable to allocate DMA buffer for READ CAPACITY(16)"
                );
                return DiskCapacity::default();
            }
        };

        // READ CAPACITY(16) returns number of sectors and sector size in bytes.
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
                        let num_sectors: u64 = capacity.ex.logical_block_address.into();
                        DiskCapacity {
                            num_sectors: num_sectors + 1, // Add one to include the last sector
                            sector_size: capacity.ex.bytes_per_block.into(),
                        }
                    }
                    _ => {
                        tracing::error!(
                            scsi_status = ?resp.scsi_status,
                            srb_status = ?resp.srb_status,
                            "READ CAPACITY(16) failed"
                        );
                        DiskCapacity::default()
                    }
                }
            }
            Err(err) => {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "READ CAPACITY(16) failed"
                );
                DiskCapacity::default()
            }
        }
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
        self.disk_capacity().num_sectors
    }

    fn sector_size(&self) -> u32 {
        self.disk_capacity().sector_size
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        // Allocate region for data in for INQUIRY (Device Identification VPD)
        // At this time we cannot allocate contiguous pages, but this could be done without an
        // assert if we could guarantee that the allocation is contiguous.
        const_assert!(
            (size_of::<scsi_defs::VpdPageHeader>()
                + size_of::<scsi_defs::VpdIdentificationDescriptor>()) as u64
                <= PAGE_SIZE_4K
        );
        let data_in_size = PAGE_SIZE_4K as usize;
        let data_in = match self.driver.allocate_dma_buffer(data_in_size) {
            Ok(buf) => buf,
            Err(err) => {
                tracing::error!(
                    error = err.to_string(),
                    "Unable to allocate DMA buffer for INQUIRY"
                );
                return None;
            }
        };

        // INQUIRY for the Device Identification VPD page returns the designator (disk ID).
        let inquiry_device_identification_cdb = scsi_defs::CdbInquiry {
            operation_code: ScsiOp::INQUIRY,
            flags: scsi_defs::InquiryFlags::new().with_vpd(true),
            page_code: scsi_defs::VPD_DEVICE_IDENTIFIERS,
            allocation_length: (data_in_size as u16).into(),
            ..FromZeros::new_zeroed()
        };

        let mut disk_id: Option<[u8; 16]> = None;
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
                                let mut created_disk_id = [0u8; 16];
                                created_disk_id[0] = designator_naa.ouid_msb;
                                created_disk_id[1..3]
                                    .copy_from_slice(designator_naa.ouid_middle.as_slice());
                                created_disk_id[3] = designator_naa.ouid_lsb;
                                created_disk_id[4..]
                                    .copy_from_slice(designator_naa.vendor_specific_id.as_slice());
                                disk_id = Some(created_disk_id);
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
        disk_id
    }

    fn physical_sector_size(&self) -> u32 {
        self.disk_capacity().sector_size
    }

    fn is_fua_respected(&self) -> bool {
        true
    }

    fn is_read_only(&self) -> bool {
        // Allocate region for data in for MODE SENSE(10)
        // At this time we cannot allocate contiguous pages, but this could be done without an
        // assert if we could guarantee that the allocation is contiguous.
        const_assert!(size_of::<scsi_defs::ModeParameterHeader10>() as u64 <= PAGE_SIZE_4K);
        let data_in_size = PAGE_SIZE_4K as usize;
        let data_in = match self.driver.allocate_dma_buffer(data_in_size) {
            Ok(buf) => buf,
            Err(err) => {
                tracing::error!(
                    error = err.to_string(),
                    "Unable to allocate DMA buffer for MODE SENSE(10)"
                );
                return false;
            }
        };

        // MODE SENSE(10) to get whether read-only. This is in the header, so it doesn't matter which page we request.
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
                    mode_header.device_specific_parameter & scsi_defs::MODE_DSP_WRITE_PROTECT != 0
                }
                _ => {
                    tracing::error!(
                        scsi_status = ?resp.scsi_status,
                        srb_status = ?resp.srb_status,
                        "MODE SENSE(10) failed"
                    );
                    false
                }
            },
            Err(err) => {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "MODE SENSE(10) failed"
                );
                false
            }
        }
    }

    async fn read_vectored(
        &self,
        buffers: &scsi_buffers::RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        let sector_size = self.disk_capacity().sector_size;
        if sector_size == 0 {
            // Failed to get sector size.
            return Err(DiskError::IllegalBlock);
        }

        if buffers.len() % sector_size as usize != 0 {
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
            transfer_blocks: (buffers.len() as u32 / sector_size).into(),
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
        let sector_size = self.disk_capacity().sector_size;
        if sector_size == 0 {
            // Failed to get sector size.
            return Err(DiskError::IllegalBlock);
        }

        if buffers.len() % sector_size as usize != 0 {
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
            transfer_blocks: (buffers.len() as u32 / sector_size).into(),
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

        // At this time we cannot allocate contiguous pages, but this could be done without an
        // assert if we could guarantee that the allocation is contiguous.
        const_assert!(
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
        // Allocate region for data in for INQUIRY (Block Limits VPD)
        // At this time we cannot allocate contiguous pages, but this could be done without an
        // assert if we could guarantee that the allocation is contiguous.
        const_assert!(size_of::<scsi_defs::VpdPageHeader>() as u64 <= PAGE_SIZE_4K);
        let data_in_size = PAGE_SIZE_4K as usize;
        let data_in = match self.driver.allocate_dma_buffer(data_in_size) {
            Ok(buf) => buf,
            Err(err) => {
                tracing::error!(
                    error = err.to_string(),
                    "Unable to allocate DMA buffer for INQUIRY"
                );
                return 0;
            }
        };

        // INQUIRY for the Supported Pages VPD page to see if Block Limits VPD is supported.
        let inquiry_supported_pages_cdb = scsi_defs::CdbInquiry {
            operation_code: ScsiOp::INQUIRY,
            flags: scsi_defs::InquiryFlags::new().with_vpd(true),
            page_code: scsi_defs::VPD_SUPPORTED_PAGES,
            allocation_length: (data_in_size as u16).into(),
            ..FromZeros::new_zeroed()
        };

        let mut optimal_unmap_size: u32 = 0;
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
                                        optimal_unmap_size =
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

        optimal_unmap_size
    }

    async fn wait_resize(&self, sector_count: u64) -> u64 {
        loop {
            let listen = self.resize_event.listen();
            let current = self.sector_count();
            if current != sector_count {
                break current;
            }
            listen.await;
        }
    }
}
