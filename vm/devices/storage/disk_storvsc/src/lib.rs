// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Disk backend implementation that uses a user-mode storvsc driver.

#![cfg(unix)]
#![forbid(unsafe_code)]

use anyhow::Context as _;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::UnmapBehavior;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
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
    /// When true, use DMA bounce buffers for I/O (required for CVM/isolated VMs
    /// where guest memory is encrypted). When false, pass guest GPNs directly
    /// in GPA Direct packets (zero-copy).
    use_bounce_buffer: bool,
    // Pre-fetched metadata: queried once at construction to avoid block_on in
    // sync DiskIo methods. Capacity is refetched on resize via wait_resize().
    sector_count: u64,
    sector_size: u32,
    disk_id: Option<[u8; 16]>,
    read_only: bool,
    optimal_unmap_sectors: u32,
    /// SCSI peripheral device type (0x00=disk, 0x05=CD-ROM).
    /// Used to select the appropriate READ_CAPACITY CDB variant.
    device_type: u8,
}

#[derive(Default)]
struct DiskCapacity {
    num_sectors: u64,
    sector_size: u32,
}

impl StorvscDisk {
    /// Creates a new storvsc-backed disk that uses the provided storvsc driver.
    ///
    /// This is async because it pre-fetches disk metadata (capacity, disk ID,
    /// read-only state, unmap granularity) from the host via SCSI commands.
    /// The DiskIo trait requires these as synchronous methods, so we cache them
    /// here to avoid `block_on` deadlocks in async executor contexts.
    pub async fn new(
        driver: Arc<StorvscDriver<MappedRingMem>>,
        lun: u8,
        use_bounce_buffer: bool,
    ) -> anyhow::Result<Self> {
        let resize_event = Arc::new(event_listener::Event::new());
        driver
            .add_resize_listener(lun, resize_event.clone())
            .context("failed to add resize listener to storvsc driver")?;
        let mut disk = Self {
            driver,
            lun,
            resize_event,
            use_bounce_buffer,
            sector_count: 0,
            sector_size: 0,
            disk_id: None,
            read_only: false,
            optimal_unmap_sectors: 0,
            device_type: 0,
        };
        // Pre-fetch metadata while we're in an async context.
        // Device type determines which commands to issue below.
        disk.device_type = disk
            .fetch_device_type()
            .await
            .context("failed to query device type")?;

        // CD-ROM/DVD (0x05) only supports READ_CAPACITY(10) and a subset
        // of VPD pages. Skip disk-specific queries for optical devices.
        let is_optical = disk.device_type == 0x05;

        if is_optical {
            // Optical devices: only need capacity for read I/O. SimpleScsiDvd
            // handles SCSI protocol (INQUIRY, MODE_SENSE, VPD) itself and
            // only delegates read_vectored/eject to the backing DiskIo.
            let capacity = disk
                .fetch_capacity_10()
                .await
                .context("failed to query disk capacity")?;
            disk.sector_count = capacity.num_sectors;
            disk.sector_size = capacity.sector_size;
            disk.disk_id = None;
            disk.read_only = true;
            disk.optimal_unmap_sectors = 0;
        } else {
            // Disk devices: try 16-byte capacity first (64-bit LBA),
            // fall back to 10-byte for older devices.
            let capacity = match disk.fetch_capacity_16().await {
                Ok(cap) => cap,
                Err(e) => {
                    tracing::warn!(
                        error = format!("{e:#}").as_str(),
                        "READ CAPACITY(16) failed, trying READ CAPACITY(10)"
                    );
                    disk.fetch_capacity_10()
                        .await
                        .context("failed to query disk capacity")?
                }
            };
            disk.sector_count = capacity.num_sectors;
            disk.sector_size = capacity.sector_size;

            disk.disk_id = disk
                .fetch_disk_id()
                .await
                .context("failed to query disk ID")?;

            disk.read_only = disk
                .fetch_read_only()
                .await
                .context("failed to query read-only state")?;

            disk.optimal_unmap_sectors = disk
                .fetch_optimal_unmap_sectors()
                .await
                .context("failed to query optimal unmap granularity")?;
        }

        Ok(disk)
    }
}

impl StorvscDisk {
    /// Fetches the SCSI peripheral device type via standard INQUIRY.
    ///
    /// Returns the low 5 bits of byte 0 (0x00=disk, 0x05=CD-ROM/DVD).
    async fn fetch_device_type(&self) -> anyhow::Result<u8> {
        let data_in_size = PAGE_SIZE_4K as usize;
        let data_in = self
            .driver
            .allocate_dma_buffer(data_in_size)
            .context("failed to allocate DMA buffer for INQUIRY")?;

        let cdb = scsi_defs::CdbInquiry {
            operation_code: ScsiOp::INQUIRY,
            allocation_length: (data_in_size as u16).into(),
            ..FromZeros::new_zeroed()
        };

        match self
            .send_scsi_request(
                cdb.as_bytes(),
                cdb.operation_code,
                data_in.pfns(),
                data_in_size,
                true,
                0,
            )
            .await
        {
            Ok(resp) if resp.scsi_status == ScsiStatus::GOOD => {
                Ok(data_in.read_obj::<u8>(0) & 0x1F)
            }
            Ok(resp) => {
                anyhow::bail!(
                    "INQUIRY failed: scsi_status={:?} srb_status={:?}",
                    resp.scsi_status,
                    resp.srb_status
                );
            }
            Err(err) => Err(err).context("INQUIRY failed"),
        }
    }

    /// Fetches capacity via READ_CAPACITY(10) -- works for all device types
    /// including CD-ROM/DVD. Returns 32-bit LBA (max ~2 TiB).
    async fn fetch_capacity_10(&self) -> anyhow::Result<DiskCapacity> {
        const_assert!(size_of::<scsi_defs::ReadCapacityData>() as u64 <= PAGE_SIZE_4K);
        let data_in_size = PAGE_SIZE_4K as usize;
        let data_in = self
            .driver
            .allocate_dma_buffer(data_in_size)
            .context("failed to allocate DMA buffer for READ CAPACITY(10)")?;

        let cdb = scsi_defs::Cdb10 {
            operation_code: ScsiOp::READ_CAPACITY,
            ..FromZeros::new_zeroed()
        };

        match self
            .send_scsi_request(
                cdb.as_bytes(),
                cdb.operation_code,
                data_in.pfns(),
                data_in_size,
                true,
                0,
            )
            .await
        {
            Ok(resp) if resp.scsi_status == ScsiStatus::GOOD => {
                let cap = data_in.read_obj::<scsi_defs::ReadCapacityData>(0);
                Ok(DiskCapacity {
                    num_sectors: u32::from(cap.logical_block_address) as u64 + 1,
                    sector_size: cap.bytes_per_block.into(),
                })
            }
            Ok(resp) => {
                anyhow::bail!(
                    "READ CAPACITY(10) failed: scsi_status={:?}, srb_status={:?}",
                    resp.scsi_status,
                    resp.srb_status
                )
            }
            Err(err) => Err(err).context("READ CAPACITY(10) failed"),
        }
    }

    /// Fetches capacity via READ_CAPACITY(16) -- 64-bit LBA for large disks.
    /// Not supported by CD-ROM/DVD devices.
    async fn fetch_capacity_16(&self) -> anyhow::Result<DiskCapacity> {
        const_assert!(size_of::<scsi_defs::ReadCapacity16Data>() as u64 <= PAGE_SIZE_4K);
        let data_in_size = PAGE_SIZE_4K as usize;
        let data_in = self
            .driver
            .allocate_dma_buffer(data_in_size)
            .context("failed to allocate DMA buffer for READ CAPACITY(16)")?;

        let cdb = scsi_defs::ServiceActionIn16 {
            operation_code: ScsiOp::READ_CAPACITY16,
            service_action: scsi_defs::SERVICE_ACTION_READ_CAPACITY16,
            allocation_length: (data_in_size as u32).into(),
            ..FromZeros::new_zeroed()
        };

        match self
            .send_scsi_request(
                cdb.as_bytes(),
                cdb.operation_code,
                data_in.pfns(),
                data_in_size,
                true,
                0,
            )
            .await
        {
            Ok(resp) if resp.scsi_status == ScsiStatus::GOOD => {
                let cap = data_in.read_obj::<scsi_defs::ReadCapacity16Data>(0);
                let num_sectors: u64 = cap.ex.logical_block_address.into();
                Ok(DiskCapacity {
                    num_sectors: num_sectors + 1,
                    sector_size: cap.ex.bytes_per_block.into(),
                })
            }
            Ok(resp) => {
                anyhow::bail!(
                    "READ CAPACITY(16) failed: scsi_status={:?}, srb_status={:?}",
                    resp.scsi_status,
                    resp.srb_status
                )
            }
            Err(err) => Err(err).context("READ CAPACITY(16) failed"),
        }
    }

    /// Fetches the disk ID via INQUIRY VPD Device Identification page.
    async fn fetch_disk_id(&self) -> anyhow::Result<Option<[u8; 16]>> {
        // Must fit in a single page -- DMA allocations may not be
        // physically contiguous across page boundaries.
        const_assert!(
            (size_of::<scsi_defs::VpdPageHeader>()
                + size_of::<scsi_defs::VpdIdentificationDescriptor>()) as u64
                <= PAGE_SIZE_4K
        );
        let data_in_size = PAGE_SIZE_4K as usize;
        let data_in = self
            .driver
            .allocate_dma_buffer(data_in_size)
            .context("failed to allocate DMA buffer for INQUIRY VPD")?;

        let cdb = scsi_defs::CdbInquiry {
            operation_code: ScsiOp::INQUIRY,
            flags: scsi_defs::InquiryFlags::new().with_vpd(true),
            page_code: scsi_defs::VPD_DEVICE_IDENTIFIERS,
            allocation_length: (data_in_size as u16).into(),
            ..FromZeros::new_zeroed()
        };

        match self
            .send_scsi_request(
                cdb.as_bytes(),
                cdb.operation_code,
                data_in.pfns(),
                data_in_size,
                true,
                0,
            )
            .await
        {
            Ok(resp) => match resp.scsi_status {
                ScsiStatus::GOOD => {
                    let mut buf_pos = 0;
                    let vpd_header = data_in.read_obj::<scsi_defs::VpdPageHeader>(0);
                    buf_pos += size_of::<scsi_defs::VpdPageHeader>();
                    while buf_pos < vpd_header.page_length as usize + 4 {
                        let designator_header =
                            data_in.read_obj::<scsi_defs::VpdIdentificationDescriptor>(buf_pos);
                        match designator_header.identifiertype {
                            scsi_defs::VPD_IDENTIFIER_TYPE_FCPH_NAME => {
                                // VpdNaaId includes VpdIdentificationDescriptor as its
                                // first field (`scsi_defs::VpdNaaId::header`), so read
                                // the full struct from the descriptor start position.
                                let designator_naa =
                                    data_in.read_obj::<scsi_defs::VpdNaaId>(buf_pos);
                                let mut created_disk_id = [0u8; 16];
                                created_disk_id[0] = designator_naa.ouid_msb;
                                created_disk_id[1..3]
                                    .copy_from_slice(designator_naa.ouid_middle.as_slice());
                                created_disk_id[3] = designator_naa.ouid_lsb;
                                created_disk_id[4..]
                                    .copy_from_slice(designator_naa.vendor_specific_id.as_slice());
                                return Ok(Some(created_disk_id));
                            }
                            _ => {
                                buf_pos += size_of::<scsi_defs::VpdIdentificationDescriptor>()
                                    + designator_header.identifier_length as usize;
                            }
                        }
                    }
                    Ok(None)
                }
                _ => {
                    anyhow::bail!(
                        "INQUIRY for Device Identification VPD failed: scsi_status={:?} srb_status={:?}",
                        resp.scsi_status,
                        resp.srb_status
                    );
                }
            },
            Err(err) => Err(err).context("INQUIRY for Device Identification VPD failed"),
        }
    }

    /// Fetches read-only state via MODE SENSE(10).
    async fn fetch_read_only(&self) -> anyhow::Result<bool> {
        // Must fit in a single page -- DMA allocations may not be
        // physically contiguous across page boundaries.
        const_assert!(size_of::<scsi_defs::ModeParameterHeader10>() as u64 <= PAGE_SIZE_4K);
        let data_in_size = PAGE_SIZE_4K as usize;
        let data_in = self
            .driver
            .allocate_dma_buffer(data_in_size)
            .context("failed to allocate DMA buffer for MODE SENSE(10)")?;

        let cdb = scsi_defs::ModeSense10 {
            operation_code: ScsiOp::MODE_SENSE10,
            flags2: scsi_defs::ModeSenseFlags::new().with_page_code(scsi_defs::MODE_PAGE_ALL),
            sub_page_code: 0,
            allocation_length: (data_in_size as u16).into(),
            ..FromZeros::new_zeroed()
        };

        match self
            .send_scsi_request(
                cdb.as_bytes(),
                cdb.operation_code,
                data_in.pfns(),
                data_in_size,
                true,
                0,
            )
            .await
        {
            Ok(resp) => match resp.scsi_status {
                ScsiStatus::GOOD => {
                    let mode_header = data_in.read_obj::<scsi_defs::ModeParameterHeader10>(0);
                    Ok(
                        mode_header.device_specific_parameter & scsi_defs::MODE_DSP_WRITE_PROTECT
                            != 0,
                    )
                }
                _ => {
                    anyhow::bail!(
                        "MODE SENSE(10) failed: scsi_status={:?} srb_status={:?}",
                        resp.scsi_status,
                        resp.srb_status
                    );
                }
            },
            Err(err) => Err(err).context("MODE SENSE(10) failed"),
        }
    }

    /// Fetches optimal unmap granularity via INQUIRY VPD Block Limits.
    ///
    /// Queries Block Limits VPD directly. Returns 0 if the device doesn't
    /// support it (e.g., DVD/CD-ROM).
    async fn fetch_optimal_unmap_sectors(&self) -> anyhow::Result<u32> {
        // Must fit in a single page -- DMA allocations may not be
        // physically contiguous across page boundaries.
        const_assert!(size_of::<scsi_defs::VpdBlockLimitsDescriptor>() as u64 <= PAGE_SIZE_4K);
        let data_in_size = PAGE_SIZE_4K as usize;
        let data_in = self
            .driver
            .allocate_dma_buffer(data_in_size)
            .context("failed to allocate DMA buffer for INQUIRY Block Limits")?;

        let cdb = scsi_defs::CdbInquiry {
            operation_code: ScsiOp::INQUIRY,
            flags: scsi_defs::InquiryFlags::new().with_vpd(true),
            page_code: scsi_defs::VPD_BLOCK_LIMITS,
            allocation_length: (data_in_size as u16).into(),
            ..FromZeros::new_zeroed()
        };

        match self
            .send_scsi_request(
                cdb.as_bytes(),
                cdb.operation_code,
                data_in.pfns(),
                data_in_size,
                true,
                0,
            )
            .await
        {
            Ok(resp) if resp.scsi_status == ScsiStatus::GOOD => {
                let block_limits = data_in.read_obj::<scsi_defs::VpdBlockLimitsDescriptor>(0);
                Ok(block_limits.optimal_unmap_granularity.into())
            }
            Ok(resp) => {
                // Device doesn't support Block Limits VPD (e.g., DVD).
                // CHECK_CONDITION with ILLEGAL REQUEST is expected here.
                tracing::debug!(
                    scsi_status = ?resp.scsi_status,
                    "Block Limits VPD not supported, unmap disabled"
                );
                Ok(0)
            }
            Err(err) => Err(err).context("INQUIRY for Block Limits VPD failed"),
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
        buf_gpns: &[u64],
        byte_len: usize,
        is_read: bool,
        gpn_offset: usize,
    ) -> Result<storvsp_protocol::ScsiRequest, DiskError> {
        let request = self.generate_scsi_request(byte_len as u32, cdb, is_read);

        let mut num_tries = 0;
        loop {
            match self
                .driver
                .send_request(&request, buf_gpns, byte_len, gpn_offset)
                .await
            {
                Ok(resp) => match resp.scsi_status {
                    ScsiStatus::GOOD => break Ok(resp), // Request succeeded, break out of loop
                    _ => {
                        tracelimit::error_ratelimited!(?op, scsi_status = ?resp.scsi_status, "SCSI request failed");
                        Err(DiskError::Io(std::io::Error::other(format!(
                            "SCSI request failed, op={:?}, scsi_status={:?}, srb_status={:?}",
                            op, resp.scsi_status, resp.srb_status
                        ))))
                    }
                },
                Err(err) => {
                    tracelimit::error_ratelimited!(
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
        self.sector_count
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
        let sector_size = self.sector_size;
        if sector_size == 0 {
            // Failed to get sector size.
            return Err(DiskError::IllegalBlock);
        }

        if !buffers.len().is_multiple_of(sector_size as usize) {
            // Buffer length must be a multiple of sector size.
            return Err(DiskError::InvalidInput);
        }

        let cdb = scsi_defs::Cdb16 {
            operation_code: ScsiOp::READ16,
            logical_block: sector.into(),
            transfer_blocks: (buffers.len() as u32 / sector_size).into(),
            ..FromZeros::new_zeroed()
        };

        if self.use_bounce_buffer {
            // CVM/isolated path: must use DMA bounce buffer because guest
            // memory is encrypted and the host can't access it directly.
            let dma_buf = self
                .driver
                .allocate_dma_buffer(buffers.len())
                .map_err(|e| DiskError::Io(std::io::Error::other(e)))?;

            let result = self
                .send_scsi_request(
                    cdb.as_bytes(),
                    cdb.operation_code,
                    dma_buf.pfns(),
                    buffers.len(),
                    true,
                    0,
                )
                .await;

            if result.is_ok() {
                let mut data = vec![0u8; buffers.len()];
                dma_buf.read_at(0, &mut data);
                let mut writer = buffers.writer();
                writer.write(&data)?;
            }

            result.map(|_| ())
        } else {
            // Non-CVM zero-copy path: pass guest GPNs directly in the GPA
            // Direct packet. The host/hypervisor can access guest memory,
            // so no bounce buffer or copy needed.
            let range = buffers.range();
            self.send_scsi_request(
                cdb.as_bytes(),
                cdb.operation_code,
                range.gpns(),
                range.len(),
                true,
                range.offset(),
            )
            .await
            .map(|_| ())
        }
    }

    async fn write_vectored(
        &self,
        buffers: &scsi_buffers::RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        let sector_size = self.sector_size;
        if sector_size == 0 {
            // Failed to get sector size.
            return Err(DiskError::IllegalBlock);
        }

        if !buffers.len().is_multiple_of(sector_size as usize) {
            // Buffer length must be a multiple of sector size.
            return Err(DiskError::InvalidInput);
        }

        let cdb = scsi_defs::Cdb16 {
            operation_code: ScsiOp::WRITE16,
            flags: scsi_defs::Cdb16Flags::new().with_fua(fua),
            logical_block: sector.into(),
            transfer_blocks: (buffers.len() as u32 / sector_size).into(),
            ..FromZeros::new_zeroed()
        };

        if self.use_bounce_buffer {
            // CVM/isolated path: bounce through DMA buffer.
            let dma_buf = self
                .driver
                .allocate_dma_buffer(buffers.len())
                .map_err(|e| DiskError::Io(std::io::Error::other(e)))?;

            let mut data = vec![0u8; buffers.len()];
            let mut reader = buffers.reader();
            reader.read(&mut data)?;
            dma_buf.write_at(0, &data);

            self.send_scsi_request(
                cdb.as_bytes(),
                cdb.operation_code,
                dma_buf.pfns(),
                buffers.len(),
                false,
                0,
            )
            .await
            .map(|_| ())
        } else {
            // Non-CVM zero-copy path: pass guest GPNs directly.
            let range = buffers.range();
            self.send_scsi_request(
                cdb.as_bytes(),
                cdb.operation_code,
                range.gpns(),
                range.len(),
                false,
                range.offset(),
            )
            .await
            .map(|_| ())
        }
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        let cdb = scsi_defs::Cdb16 {
            operation_code: ScsiOp::SYNCHRONIZE_CACHE16,
            logical_block: 0.into(),
            transfer_blocks: 0.into(), // 0 indicates to sync all sectors
            ..FromZeros::new_zeroed()
        };

        self.send_scsi_request(cdb.as_bytes(), cdb.operation_code, &[], 0, false, 0)
            .await
            .map(|_| ())
    }

    async fn eject(&self) -> Result<(), DiskError> {
        let cdb = scsi_defs::StartStop {
            operation_code: ScsiOp::START_STOP_UNIT,
            flag: scsi_defs::StartStopFlags::new().with_load_eject(true),
            ..FromZeros::new_zeroed()
        };

        self.send_scsi_request(cdb.as_bytes(), cdb.operation_code, &[], 0, false, 0)
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
            allocation_length: ((size_of::<scsi_defs::UnmapListHeader>()
                + size_of::<scsi_defs::UnmapBlockDescriptor>())
                as u16)
                .into(),
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
            lba_count: u32::try_from(count)
                .map_err(|_| DiskError::InvalidInput)?
                .into(),
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

        let param_list_len =
            size_of::<scsi_defs::UnmapListHeader>() + size_of::<scsi_defs::UnmapBlockDescriptor>();
        self.send_scsi_request(
            cdb.as_bytes(),
            cdb.operation_code,
            data_out.pfns(),
            param_list_len,
            false,
            0,
        )
        .await
        .map(|_| ())
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        if self.optimal_unmap_sectors == 0 {
            UnmapBehavior::Ignored
        } else {
            UnmapBehavior::Unspecified
        }
    }

    fn optimal_unmap_sectors(&self) -> u32 {
        self.optimal_unmap_sectors
    }

    // TODO: Add unit tests for wait_resize -- cover error retry with
    // listen.await backoff, and capacity change detection.
    async fn wait_resize(&self, sector_count: u64) -> u64 {
        loop {
            let listen = self.resize_event.listen();
            // Refetch capacity from host (we're in async context here)
            let capacity = match self.fetch_capacity_10().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(
                        error = e.as_ref() as &dyn std::error::Error,
                        "failed to refetch capacity on resize"
                    );
                    listen.await;
                    continue;
                }
            };
            if capacity.num_sectors != sector_count {
                break capacity.num_sectors;
            }
            listen.await;
        }
    }
}
