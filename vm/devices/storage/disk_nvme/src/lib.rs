// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Disk backend implementation that uses a user-mode NVMe driver based on VFIO.

#![cfg(any(windows, target_os = "linux"))]
#![forbid(unsafe_code)]
#![expect(missing_docs)]

use async_trait::async_trait;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::MediumErrorDetails;
use disk_backend::pr;
use inspect::Inspect;
use nvme_common::from_nvme_reservation_report;
use nvme_spec::Status;
use nvme_spec::nvm;
#[cfg(target_os = "linux")]
use pal::unix::affinity::get_cpu_number;
#[cfg(windows)]
use pal::windows::affinity::get_cpu_number;
use std::io;
use std::sync::Arc;

#[derive(Debug, Inspect)]
pub struct NvmeDisk {
    /// NVMe namespace mapped to the disk representation.
    #[inspect(flatten)]
    namespace: Arc<nvme_driver::Namespace>,
    #[inspect(skip)]
    block_shift: u32,
}

impl NvmeDisk {
    pub fn new(namespace: Arc<nvme_driver::Namespace>) -> Self {
        Self {
            block_shift: namespace.block_size().trailing_zeros(),
            namespace,
        }
    }
}

impl DiskIo for NvmeDisk {
    fn disk_type(&self) -> &str {
        "nvme"
    }

    fn sector_count(&self) -> u64 {
        self.namespace.block_count()
    }

    fn sector_size(&self) -> u32 {
        self.namespace.block_size()
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        None // TODO
    }

    fn physical_sector_size(&self) -> u32 {
        4096 // TODO
    }

    fn is_fua_respected(&self) -> bool {
        // NVMe does not provide a way to specify that FUA is ignored.
        true
    }

    fn is_read_only(&self) -> bool {
        false // TODO
    }

    fn pr(&self) -> Option<&dyn pr::PersistentReservation> {
        (u8::from(self.namespace.reservation_capabilities()) != 0).then_some(self)
    }

    async fn read_vectored(
        &self,
        buffers: &scsi_buffers::RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        let block_count = buffers.len() as u64 >> self.block_shift;
        let mut block_offset = 0;
        while block_offset < block_count {
            let this_block_count = (block_count - block_offset)
                .min(self.namespace.max_transfer_block_count().into())
                as u32;

            self.namespace
                .read(
                    get_cpu_number(),
                    sector + block_offset,
                    this_block_count,
                    buffers.guest_memory(),
                    buffers.range().subrange(
                        (block_offset as usize) << self.block_shift,
                        (this_block_count as usize) << self.block_shift,
                    ),
                )
                .await
                .map_err(map_nvme_error)?;

            block_offset += this_block_count as u64;
        }
        Ok(())
    }

    async fn write_vectored(
        &self,
        buffers: &scsi_buffers::RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        let block_count = buffers.len() as u64 >> self.block_shift;
        let mut block_offset = 0;
        while block_offset < block_count {
            let this_block_count = (block_count - block_offset)
                .min(self.namespace.max_transfer_block_count().into())
                as u32;

            self.namespace
                .write(
                    get_cpu_number(),
                    sector + block_offset,
                    this_block_count,
                    fua,
                    buffers.guest_memory(),
                    buffers.range().subrange(
                        (block_offset as usize) << self.block_shift,
                        (this_block_count as usize) << self.block_shift,
                    ),
                )
                .await
                .map_err(map_nvme_error)?;

            block_offset += this_block_count as u64;
        }
        Ok(())
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        self.namespace
            .flush(get_cpu_number())
            .await
            .map_err(map_nvme_error)?;
        Ok(())
    }

    async fn wait_resize(&self, sector_count: u64) -> u64 {
        self.namespace.wait_resize(sector_count).await
    }

    async fn unmap(
        &self,
        sector_offset: u64,
        sector_count: u64,
        _block_level_only: bool,
    ) -> Result<(), DiskError> {
        if !self.namespace.supports_dataset_management() {
            return Ok(());
        }
        let mut processed = 0;
        let max = self.namespace.dataset_management_range_size_limit();
        while processed < sector_count {
            let lba_count = (sector_count - processed).min(max.into());
            self.namespace
                .deallocate(
                    get_cpu_number(),
                    &[nvm::DsmRange {
                        context_attributes: 0,
                        lba_count: lba_count as u32,
                        starting_lba: sector_offset + processed,
                    }],
                )
                .await
                .map_err(map_nvme_error)?;

            processed += lba_count;
        }
        Ok(())
    }

    fn unmap_behavior(&self) -> disk_backend::UnmapBehavior {
        if self.namespace.supports_dataset_management() {
            disk_backend::UnmapBehavior::Unspecified
        } else {
            disk_backend::UnmapBehavior::Ignored
        }
    }

    fn optimal_unmap_sectors(&self) -> u32 {
        self.namespace.preferred_deallocate_granularity().into()
    }
}

#[async_trait]
impl pr::PersistentReservation for NvmeDisk {
    fn capabilities(&self) -> pr::ReservationCapabilities {
        nvme_common::from_nvme_reservation_capabilities(self.namespace.reservation_capabilities())
    }

    async fn report(&self) -> Result<pr::ReservationReport, DiskError> {
        let (report, controllers) = self
            .namespace
            .reservation_report_extended(get_cpu_number())
            .await
            .map_err(map_nvme_error)?;

        from_nvme_reservation_report(&report.report, &controllers)
            .map_err(|err| DiskError::Io(io::Error::new(io::ErrorKind::InvalidInput, err)))
    }

    async fn register(
        &self,
        current_key: Option<u64>,
        new_key: u64,
        ptpl: Option<bool>,
    ) -> Result<(), DiskError> {
        let action = if new_key == 0 {
            nvm::ReservationRegisterAction::UNREGISTER
        } else if current_key.is_some() {
            nvm::ReservationRegisterAction::REPLACE
        } else {
            nvm::ReservationRegisterAction::REGISTER
        };
        self.namespace
            .reservation_register(get_cpu_number(), action, current_key, new_key, ptpl)
            .await
            .map_err(map_nvme_error)?;

        Ok(())
    }

    async fn reserve(
        &self,
        key: u64,
        reservation_type: pr::ReservationType,
    ) -> Result<(), DiskError> {
        self.namespace
            .reservation_acquire(
                get_cpu_number(),
                nvm::ReservationAcquireAction::ACQUIRE,
                key,
                0,
                nvme_common::to_nvme_reservation_type(reservation_type),
            )
            .await
            .map_err(map_nvme_error)?;

        Ok(())
    }

    async fn release(
        &self,
        key: u64,
        reservation_type: pr::ReservationType,
    ) -> Result<(), DiskError> {
        self.namespace
            .reservation_release(
                get_cpu_number(),
                nvm::ReservationReleaseAction::RELEASE,
                key,
                nvme_common::to_nvme_reservation_type(reservation_type),
            )
            .await
            .map_err(map_nvme_error)?;

        Ok(())
    }

    async fn clear(&self, key: u64) -> Result<(), DiskError> {
        self.namespace
            .reservation_release(
                get_cpu_number(),
                nvm::ReservationReleaseAction::CLEAR,
                key,
                nvm::ReservationType(0),
            )
            .await
            .map_err(map_nvme_error)?;

        Ok(())
    }

    async fn preempt(
        &self,
        current_key: u64,
        preempt_key: u64,
        reservation_type: pr::ReservationType,
        abort: bool,
    ) -> Result<(), DiskError> {
        self.namespace
            .reservation_acquire(
                get_cpu_number(),
                if abort {
                    nvm::ReservationAcquireAction::PREEMPT_AND_ABORT
                } else {
                    nvm::ReservationAcquireAction::PREEMPT
                },
                current_key,
                preempt_key,
                nvme_common::to_nvme_reservation_type(reservation_type),
            )
            .await
            .map_err(map_nvme_error)?;

        Ok(())
    }
}

fn map_nvme_error(err: nvme_driver::RequestError) -> DiskError {
    match err {
        err @ nvme_driver::RequestError::Gone(_) => {
            DiskError::Io(io::Error::new(io::ErrorKind::NotConnected, err))
        }
        nvme_driver::RequestError::Nvme(err) => {
            match err.status() {
                Status::RESERVATION_CONFLICT => DiskError::ReservationConflict,

                Status::INVALID_FIELD_IN_COMMAND => DiskError::InvalidInput,

                Status::LBA_OUT_OF_RANGE => DiskError::IllegalBlock,

                // MediumError
                Status::DATA_TRANSFER_ERROR | Status::CAPACITY_EXCEEDED => {
                    DiskError::Io(io::Error::other(err))
                }
                Status::MEDIA_WRITE_FAULT => {
                    DiskError::MediumError(io::Error::other(err), MediumErrorDetails::WriteFault)
                }
                Status::MEDIA_UNRECOVERED_READ_ERROR => DiskError::MediumError(
                    io::Error::other(err),
                    MediumErrorDetails::UnrecoveredReadError,
                ),
                Status::MEDIA_END_TO_END_GUARD_CHECK_ERROR => DiskError::MediumError(
                    io::Error::other(err),
                    MediumErrorDetails::GuardCheckFailed,
                ),
                Status::MEDIA_END_TO_END_APPLICATION_TAG_CHECK_ERROR => DiskError::MediumError(
                    io::Error::other(err),
                    MediumErrorDetails::ApplicationTagCheckFailed,
                ),
                Status::MEDIA_END_TO_END_REFERENCE_TAG_CHECK_ERROR => DiskError::MediumError(
                    io::Error::other(err),
                    MediumErrorDetails::ReferenceTagCheckFailed,
                ),

                Status::COMMAND_ABORTED_DUE_TO_PREEMPT_AND_ABORT => {
                    DiskError::AbortDueToPreemptAndAbort
                }

                _ => DiskError::Io(io::Error::other(err)),
            }
        }
        nvme_driver::RequestError::Memory(err) => DiskError::MemoryAccess(err.into()),
        err @ nvme_driver::RequestError::TooLarge => {
            DiskError::Io(io::Error::new(io::ErrorKind::InvalidInput, err))
        }
    }
}
