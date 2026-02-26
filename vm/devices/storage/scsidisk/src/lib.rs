// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod atapi_scsi;
mod getlbastatus;
mod inquiry;
mod reservation;
pub mod resolver;
pub mod scsidvd;
mod unmap;

#[cfg(test)]
mod tests;

pub use inquiry::INQUIRY_DATA_TEMPLATE;

use disk_backend::Disk;
use disk_backend::DiskError;
use disk_backend::UnmapBehavior;
use guestmem::AccessError;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use guid::Guid;
use inspect::Inspect;
use parking_lot::Mutex;
use scsi::AdditionalSenseCode;
use scsi::ScsiOp;
use scsi::ScsiStatus;
use scsi::SenseKey;
use scsi::srb::SrbStatus;
use scsi_buffers::RequestBuffers;
use scsi_core::ASYNC_SCSI_DISK_STACK_SIZE;
use scsi_core::AsyncScsiDisk;
use scsi_core::Request;
use scsi_core::ScsiResult;
use scsi_core::ScsiSaveRestore;
use scsi_core::save_restore::SavedSenseData;
use scsi_core::save_restore::ScsiDiskSavedState;
use scsi_core::save_restore::ScsiSavedState;
use scsi_defs as scsi;
use scsidisk_resources::DiskIdentity;
use scsidisk_resources::DiskParameters;
use stackfuture::StackFuture;
use std::fmt::Debug;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use thiserror::Error;
use tracing::Instrument;
use tracing_helpers::ErrorValueExt;
use unmap::validate_lba_range;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

const UNMAP_RANGE_DESCRIPTOR_COUNT_MAX: u16 = 4096;
const VHDMP_MAX_WRITE_SAME_LENGTH_BYTES: u64 = 8 * 1024 * 1024; // bytes

impl ScsiSaveRestore for SimpleScsiDisk {
    fn save(&self) -> Result<Option<ScsiSavedState>, SaveError> {
        let sense = self.sense_data.get();
        let sense_data = sense.map(|sense| SavedSenseData {
            sense_key: sense.header.sense_key.0,
            additional_sense_code: sense.additional_sense_code.0,
            additional_sense_code_qualifier: sense.additional_sense_code_qualifier,
        });
        Ok(Some(ScsiSavedState::ScsiDisk(ScsiDiskSavedState {
            sector_count: self.last_sector_count.load(Ordering::Relaxed),
            sense_data,
        })))
    }

    fn restore(&self, state: &ScsiSavedState) -> Result<(), RestoreError> {
        if let ScsiSavedState::ScsiDisk(disk_state) = state {
            let ScsiDiskSavedState {
                sector_count,
                sense_data,
            } = *disk_state;

            // restore sense data
            self.sense_data.set(
                sense_data
                    .map(|sense| {
                        scsi::SenseData::new(
                            SenseKey(sense.sense_key),
                            AdditionalSenseCode(sense.additional_sense_code),
                            sense.additional_sense_code_qualifier,
                        )
                    })
                    .as_ref(),
            );

            self.last_sector_count
                .store(sector_count, Ordering::Relaxed);
            Ok(())
        } else {
            Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                "saved state didn't match expected format ScsiDiskSavedState"
            )))
        }
    }
}

pub struct SimpleScsiDisk {
    disk: Disk,
    sector_shift: u8,
    physical_extra_shift: u8,
    sector_size: u32,
    sense_data: SenseDataSlot,
    scsi_parameters: ScsiParameters,
    support_pr: bool,
    last_sector_count: AtomicU64,
}

#[derive(Debug, Clone, Inspect)]
struct ScsiParameters {
    disk_id: [u8; 16],
    physical_sector_size: u32,
    support_fua: bool,
    write_cache_enabled: bool,
    support_odx: bool,
    support_unmap: bool,
    support_get_lba_status: bool,
    maximum_transfer_length: usize,
    identity: DiskIdentity,
    serial_number: Vec<u8>,
    medium_rotation_rate: u16,
    optimal_unmap_sectors: u32,
    atomic_parameters: disk_backend::AtomicParameters,
}

impl SimpleScsiDisk {
    pub fn new(disk: Disk, disk_parameters: DiskParameters) -> Self {
        let sector_size = disk.sector_size();
        let sector_shift = sector_size.trailing_zeros() as u8;
        let mut sector_count = disk.sector_count();

        // Update the reported disk size.
        if let Some(size) = disk_parameters.scsi_disk_size_in_bytes {
            sector_count = sector_count.min(size >> sector_shift);
        }

        // Determine the SCSI parameters from the passed-in disk parameters and
        // the information from the underlying disk.
        let scsi_parameters = {
            let DiskParameters {
                disk_id,
                identity,
                serial_number,
                medium_rotation_rate,
                physical_sector_size,
                fua,
                write_cache,
                scsi_disk_size_in_bytes: _,
                odx,
                unmap,
                max_transfer_length,
                optimal_unmap_sectors,
                get_lba_status,
            } = disk_parameters;

            fn nonzero_id(id: [u8; 16]) -> Option<[u8; 16]> {
                if id == [0; 16] { None } else { Some(id) }
            }

            // Choose the first non-zero disk ID from the passed in parameters,
            // the disk, or a new random ID.
            let disk_id = disk_id
                .and_then(nonzero_id)
                .or_else(|| disk.disk_id().and_then(nonzero_id))
                .unwrap_or_else(|| Guid::new_random().into());

            ScsiParameters {
                disk_id,
                physical_sector_size: physical_sector_size
                    .unwrap_or_else(|| disk.physical_sector_size()),
                support_fua: fua.unwrap_or_else(|| disk.is_fua_respected()),
                write_cache_enabled: write_cache.unwrap_or(true),
                support_odx: odx.unwrap_or(false),
                support_get_lba_status: get_lba_status,
                support_unmap: unmap.unwrap_or(disk.unmap_behavior() != UnmapBehavior::Ignored),
                maximum_transfer_length: max_transfer_length.unwrap_or(8 * 1024 * 1024),
                identity: identity.unwrap_or_else(DiskIdentity::msft),
                serial_number,
                medium_rotation_rate: medium_rotation_rate.unwrap_or(1), // non-rotating media (SSD)
                optimal_unmap_sectors: optimal_unmap_sectors.unwrap_or(1),
                atomic_parameters: disk.atomic_parameters(),
            }
        };

        let physical_extra_shift =
            scsi_parameters.physical_sector_size.trailing_zeros() as u8 - sector_shift;
        let support_pr = disk.pr().is_some();

        SimpleScsiDisk {
            disk,
            sector_shift,
            physical_extra_shift,
            sector_size,
            sense_data: Default::default(),
            scsi_parameters,
            support_pr,
            last_sector_count: AtomicU64::new(sector_count),
        }
    }
}

#[derive(Error, Debug)]
enum ScsiError {
    #[error("memory access error")]
    MemoryAccess(#[source] AccessError),
    #[error("illegal request, asc: {0:?}")]
    IllegalRequest(AdditionalSenseCode),
    #[error("data overrun")]
    DataOverrun,
    #[error("srb generic error")]
    SrbError,
    #[error("device is write protected")]
    WriteProtected,
    #[error("disk io error")]
    Disk(#[source] DiskError),
    #[error("pending unit attention")]
    UnitAttention,
    #[error("unsupported mode page code: page control {0} page code {1}")]
    UnsupportedModePageCode(u8, u8),
    #[error("unsupported vpd page code: {0}")]
    UnsupportedVpdPageCode(u8),
    #[error("unsupported service action: {0}")]
    UnsupportedServiceAction(u8),
}

struct RequestParameters {
    tx: usize,
    offset: u64,
    fua: bool,
}

struct WriteSameParameters {
    lba_count: usize,
    start_lba: u64,
    fua: bool,
    sector_size: usize,
    tx: usize,
}

const MODE_CACHING_PAGE_SIZE: usize = size_of::<scsi::ModeCachingPage>();
const MODE_PARAMETER_HEADER_SIZE: usize = size_of::<scsi::ModeParameterHeader>();
const MODE_PARAMETER_HEADER10_SIZE: usize = size_of::<scsi::ModeParameterHeader10>();
const MODE_DATA_LENGTH10: u16 = (MODE_PARAMETER_HEADER10_SIZE + MODE_CACHING_PAGE_SIZE - 2) as u16;
const MODE_DATA_LENGTH: u8 = (MODE_PARAMETER_HEADER_SIZE + MODE_CACHING_PAGE_SIZE - 1) as u8;

pub fn illegal_request_sense(sense_code: AdditionalSenseCode) -> scsi::SenseData {
    match sense_code {
        AdditionalSenseCode::ILLEGAL_COMMAND
        | AdditionalSenseCode::INVALID_CDB
        | AdditionalSenseCode::NO_SENSE
        | AdditionalSenseCode::INVALID_FIELD_PARAMETER_LIST
        | AdditionalSenseCode::PARAMETER_LIST_LENGTH
        | AdditionalSenseCode::ILLEGAL_BLOCK => {
            scsi::SenseData::new(SenseKey::ILLEGAL_REQUEST, sense_code, 0)
        }
        _ => unreachable!(),
    }
}

impl SimpleScsiDisk {
    fn handle_request_sense(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
        unit_attention: bool,
    ) -> Result<usize, ScsiError> {
        let cdb = scsi::CdbInquiry::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let allocation_length = cdb.allocation_length.get() as usize;

        let min = size_of::<scsi::SenseDataHeader>();
        if allocation_length < min || allocation_length > external_data.len() {
            tracelimit::error_ratelimited!(
                allocation_length,
                min,
                external_data_len = external_data.len(),
                "srb error"
            );
            return Err(ScsiError::SrbError);
        }

        let sense = if unit_attention {
            scsi::SenseData::new(
                SenseKey::UNIT_ATTENTION,
                AdditionalSenseCode::PARAMETERS_CHANGED,
                scsi::SCSI_SENSEQ_CAPACITY_DATA_CHANGED,
            )
        } else {
            self.sense_data.take().unwrap_or_else(|| {
                scsi::SenseData::new(SenseKey::NO_SENSE, AdditionalSenseCode::NO_SENSE, 0x00)
            })
        };

        let tx = std::cmp::min(allocation_length, size_of::<scsi::SenseData>());
        external_data
            .writer()
            .write(&sense.as_bytes()[..tx])
            .map_err(ScsiError::MemoryAccess)?;

        Ok(tx)
    }

    fn handle_mode_select(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
    ) -> Result<usize, ScsiError> {
        let is_mode_select_10 = request.scsiop() == ScsiOp::MODE_SELECT10;
        let request_length;
        let header_size;
        let is_spbit_set;
        if is_mode_select_10 {
            let cdb = scsi::ModeSelect10::read_from_prefix(&request.cdb[..])
                .unwrap()
                .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
            request_length = cdb.parameter_list_length.get() as usize;
            header_size = MODE_PARAMETER_HEADER10_SIZE;
            is_spbit_set = cdb.flags.spbit();
        } else {
            let cdb = scsi::ModeSelect::read_from_prefix(&request.cdb[..])
                .unwrap()
                .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
            request_length = cdb.parameter_list_length as usize;
            header_size = MODE_PARAMETER_HEADER_SIZE;
            is_spbit_set = cdb.flags.spbit();
        }

        if request_length == 0 {
            return Ok(0);
        }

        // Validate buffer size
        let min = header_size + MODE_CACHING_PAGE_SIZE;
        if request_length != external_data.len() || request_length < min {
            tracelimit::error_ratelimited!(
                request_length,
                external_data = external_data.len(),
                min,
                "invalid parameter list length"
            );
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::PARAMETER_LIST_LENGTH,
            ));
        }

        // Don't support saving pages.
        if is_spbit_set {
            tracing::debug!("doesn't support saving pages");
            return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
        }

        let mut buffer: Vec<u8> = vec![0; request_length];
        external_data
            .reader()
            .read(&mut buffer)
            .map_err(ScsiError::MemoryAccess)?;

        let block_descriptor_length = if is_mode_select_10 {
            let temp10 = scsi::ModeParameterHeader10::read_from_prefix(
                &buffer[..MODE_PARAMETER_HEADER10_SIZE],
            )
            .unwrap()
            .0; // TODO: zerocopy: from-prefix (read_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
            usize::from(temp10.block_descriptor_length)
        } else {
            let temp =
                scsi::ModeParameterHeader::read_from_prefix(&buffer[..MODE_PARAMETER_HEADER_SIZE])
                    .unwrap()
                    .0; // TODO: zerocopy: from-prefix (read_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
            temp.block_descriptor_length as usize
        };

        // Skip block descriptor.
        let skipped = header_size + block_descriptor_length;
        let min = skipped + MODE_CACHING_PAGE_SIZE;
        if request_length < min {
            tracelimit::error_ratelimited!(request_length, min, "invalid parameter list length");
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::PARAMETER_LIST_LENGTH,
            ));
        }

        // Parse ModeCachingPage.
        let page = scsi::ModeCachingPage::read_from_prefix(
            &buffer[skipped..skipped + MODE_CACHING_PAGE_SIZE],
        )
        .unwrap()
        .0; // TODO: zerocopy: from-prefix (read_from_prefix): use-rest-of-range, zerocopy: err (https://github.com/microsoft/openvmm/issues/759)
        if page.page_code != scsi::MODE_PAGE_CACHING
            || (page.page_length as usize) < MODE_CACHING_PAGE_SIZE
            || ((page.flags & scsi::MODE_CACHING_WRITE_CACHE_ENABLE == 0)
                && self.scsi_parameters.write_cache_enabled)
            || ((page.flags & scsi::MODE_CACHING_WRITE_CACHE_ENABLE != 0)
                && !self.scsi_parameters.write_cache_enabled)
        {
            // Attempts to turn off write caching must be failed, otherwise
            // storage migration might lead to the initiator believing that it
            // has write caching turned off when in fact write caching is
            // turned on, which would be a potential data loss situation.
            //
            // Hopefully no initiator will get too annoyed when this fails.
            // The only other option would be to erroneously report success here
            // and then still report that write caching is off next time it's
            // queried, but that still leaves the initiator potentially out of
            // sync on the fact that write caching is potentially on and there's
            // nothing we can do about it.  So hopefully reporting failure here
            // works for all relevant initiators.
            tracing::debug!(
                page_code = page.page_code,
                page_length = page.page_length,
                flags = page.flags,
                write_cache_enabled = self.scsi_parameters.write_cache_enabled,
                "invalid parameter list"
            );
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::INVALID_FIELD_PARAMETER_LIST,
            ));
        }

        Ok(request_length)
    }

    fn handle_mode_sense(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
    ) -> Result<usize, ScsiError> {
        if external_data.is_empty() {
            return Ok(0);
        }

        let is_mode_sense_10 = request.scsiop() == ScsiOp::MODE_SENSE10;
        let page_code;
        let page_control;
        let allocation_length;
        let header_size;
        if is_mode_sense_10 {
            let cdb = scsi::ModeSense10::read_from_prefix(&request.cdb[..])
                .unwrap()
                .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
            allocation_length = cdb.allocation_length.get() as usize;
            page_code = cdb.flags2.page_code();
            page_control = cdb.flags2.pc() << 6;
            header_size = MODE_PARAMETER_HEADER10_SIZE;
        } else {
            let cdb = scsi::ModeSense::read_from_prefix(&request.cdb[..])
                .unwrap()
                .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
            allocation_length = cdb.allocation_length as usize;
            page_code = cdb.flags2.page_code();
            page_control = cdb.flags2.pc() << 6;
            header_size = MODE_PARAMETER_HEADER_SIZE;
        }

        // It is valid to not supply a buffer, just complete immediately.
        if allocation_length == 0 {
            return Ok(0);
        }

        // Verify that the SRB actually supplies the indicated buffer and that we have enough
        // for a single header (not sure if this is correct).
        if allocation_length > external_data.len() || allocation_length < header_size {
            tracelimit::error_ratelimited!(
                allocation_length,
                external_data = external_data.len(),
                header_size,
                "invalid cdb"
            );
            return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
        }

        if page_control == scsi::MODE_CONTROL_SAVED_VALUES
            || (page_code != scsi::MODE_PAGE_CACHING && page_code != scsi::MODE_PAGE_ALL)
        {
            return Err(ScsiError::UnsupportedModePageCode(page_control, page_code));
        }

        let mut dsp = 0;
        if self.disk.is_read_only() {
            dsp |= scsi::MODE_DSP_WRITE_PROTECT;
        }

        if self.scsi_parameters.support_fua {
            dsp |= scsi::MODE_DSP_FUA_SUPPORTED;
        }

        let temp;
        let temp10;
        let header = if is_mode_sense_10 {
            temp10 = scsi::ModeParameterHeader10 {
                mode_data_length: MODE_DATA_LENGTH10.into(),
                device_specific_parameter: dsp,
                ..FromZeros::new_zeroed()
            };
            temp10.as_bytes()
        } else {
            temp = scsi::ModeParameterHeader {
                mode_data_length: MODE_DATA_LENGTH,
                device_specific_parameter: dsp,
                ..FromZeros::new_zeroed()
            };
            temp.as_bytes()
        };

        let mut page = scsi::ModeCachingPage {
            page_code: scsi::MODE_PAGE_CACHING,
            page_length: (MODE_CACHING_PAGE_SIZE - 2) as u8,
            ..FromZeros::new_zeroed()
        };

        if (page_control == scsi::MODE_CONTROL_CURRENT_VALUES
            || page_control == scsi::MODE_CONTROL_DEFAULT_VALUES)
            && external_data.len() - header_size >= scsi::WRITE_CACHE_ENABLE_BYTE_OFFSET
        {
            if self.scsi_parameters.write_cache_enabled {
                page.flags |= scsi::MODE_CACHING_WRITE_CACHE_ENABLE;
            }
        }

        // HEADER10_SIZE > HEADER_SIZE ensures we have enough space.
        let mut data = [0; MODE_PARAMETER_HEADER10_SIZE + MODE_CACHING_PAGE_SIZE];
        data[..header_size].copy_from_slice(header);
        data[header_size..header_size + MODE_CACHING_PAGE_SIZE].copy_from_slice(page.as_bytes());
        let tx = std::cmp::min(allocation_length, header_size + MODE_CACHING_PAGE_SIZE);
        external_data
            .writer()
            .write(&data[..tx])
            .map_err(ScsiError::MemoryAccess)?;

        Ok(tx)
    }

    fn handle_service_action_in16(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
        sector_count: u64,
    ) -> Result<usize, ScsiError> {
        let cdb = scsi::ServiceActionIn16::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        match cdb.service_action & 0x1f {
            scsi::SERVICE_ACTION_READ_CAPACITY16 => {
                let min = size_of::<scsi::ReadCapacityDataEx>();
                if external_data.len() < min {
                    tracelimit::error_ratelimited!(len = external_data.len(), min, "data overrun");
                    return Err(ScsiError::DataOverrun);
                }

                let mut data = scsi::ReadCapacity16Data {
                    ex: scsi::ReadCapacityDataEx {
                        // This query wants the LBA sector index of the last sector, not the
                        // number of sectors - hence the minus one.
                        logical_block_address: (sector_count - 1).into(),
                        bytes_per_block: (1u32 << self.sector_shift).into(),
                    },
                    exponents: self.physical_extra_shift,
                    ..FromZeros::new_zeroed()
                };

                if self.scsi_parameters.support_unmap {
                    // report trim capabilities:
                    //  - trim is supported
                    //  - read zero after trim is not supported
                    data.lowest_aligned_block_msb |= scsi::READ_CAPACITY16_LBPME;
                }

                let tx = std::cmp::min(external_data.len(), size_of::<scsi::ReadCapacity16Data>());
                external_data
                    .writer()
                    .write(&data.as_bytes()[..tx])
                    .map_err(ScsiError::MemoryAccess)?;

                Ok(tx)
            }
            scsi::SERVICE_ACTION_GET_LBA_STATUS => {
                if !self.scsi_parameters.support_get_lba_status {
                    tracing::debug!("doesn't support get lba status");
                    Err(ScsiError::IllegalRequest(
                        AdditionalSenseCode::ILLEGAL_COMMAND,
                    ))
                } else {
                    self.handle_get_lba_status(external_data, request, sector_count)
                }
            }
            _ => Err(ScsiError::UnsupportedServiceAction(cdb.service_action)),
        }
    }

    fn handle_read_capacity(
        &self,
        external_data: &RequestBuffers<'_>,
        sector_count: u64,
    ) -> Result<usize, ScsiError> {
        let tx = size_of::<scsi::ReadCapacityData>();
        if external_data.len() < tx {
            tracelimit::error_ratelimited!(len = external_data.len(), tx, "data overrun");
            return Err(ScsiError::DataOverrun);
        }

        // This query wants the LBA sector index of the last sector, not the
        // number of sectors - hence the minus one.
        // If the VHD is larger than the SCSI structure can support, Report
        // the largest size possible.
        let last_lba = std::cmp::min(sector_count - 1, u32::MAX.into());
        let data = scsi::ReadCapacityData {
            logical_block_address: (last_lba as u32).into(),
            bytes_per_block: (1u32 << self.sector_shift).into(),
        };

        external_data
            .writer()
            .write(data.as_bytes())
            .map_err(ScsiError::MemoryAccess)?;

        Ok(tx)
    }

    fn handle_verify_validation(
        &self,
        request: &Request,
        sector_count: u64,
    ) -> Result<usize, ScsiError> {
        let op = request.scsiop();
        tracing::debug!("handle_verify_validation");
        let (start_lba, lba_count) = match op {
            ScsiOp::VERIFY | ScsiOp::WRITE_VERIFY => {
                let cdb = scsi::Cdb10::read_from_prefix(&request.cdb[..]).unwrap().0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
                if cdb.flags.relative_address() {
                    tracing::debug!(flags = ?cdb.flags, "doesn't support relative address");
                    return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
                }
                (
                    cdb.logical_block.get() as u64,
                    cdb.transfer_blocks.get() as u64,
                )
            }
            ScsiOp::VERIFY12 | ScsiOp::WRITE_VERIFY12 => {
                let cdb = scsi::Cdb12::read_from_prefix(&request.cdb[..]).unwrap().0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
                if cdb.flags.relative_address() {
                    tracing::debug!(flags = ?cdb.flags, "doesn't support relative address");
                    return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
                }
                (
                    cdb.logical_block.get() as u64,
                    cdb.transfer_blocks.get() as u64,
                )
            }
            ScsiOp::VERIFY16 | ScsiOp::WRITE_VERIFY16 => {
                let cdb = scsi::Cdb16::read_from_prefix(&request.cdb[..]).unwrap().0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
                (cdb.logical_block.get(), cdb.transfer_blocks.get() as u64)
            }
            _ => unreachable!(),
        };

        if !validate_lba_range(sector_count, start_lba, lba_count) {
            //valiate_lba_range trace errors
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::ILLEGAL_BLOCK,
            ));
        }

        Ok(0)
    }

    fn handle_send_diagnostic_validation(&self, request: &Request) -> Result<usize, ScsiError> {
        tracing::debug!("handle_send_diagnostic_validation");
        let cdb = scsi::SendDiagnostic::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        if cdb.flags.self_test_code() == 0
            && !cdb.flags.page_format()
            && cdb.parameter_list_length.get() == 0
        {
            Ok(0)
        } else {
            Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB))
        }
    }

    fn handle_control_cdb(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
        sector_count: u64,
    ) -> Result<usize, ScsiError> {
        let op = request.scsiop();
        match op {
            ScsiOp::INQUIRY => self.handle_inquiry(external_data, request, sector_count),
            ScsiOp::REQUEST_SENSE => self.handle_request_sense(external_data, request, false),
            ScsiOp::MODE_SENSE | ScsiOp::MODE_SENSE10 => {
                self.handle_mode_sense(external_data, request)
            }
            ScsiOp::TEST_UNIT_READY
            | ScsiOp::FORMAT_UNIT
            | ScsiOp::RESERVE_UNIT
            | ScsiOp::RELEASE_UNIT
            | ScsiOp::MEDIUM_REMOVAL => Ok(0),
            ScsiOp::SEND_DIAGNOSTIC => self.handle_send_diagnostic_validation(request),
            ScsiOp::READ_CAPACITY => self.handle_read_capacity(external_data, sector_count),
            // It's SCSIOP_READ_CAPACITY16 in vhdmp
            ScsiOp::SERVICE_ACTION_IN16 => {
                self.handle_service_action_in16(external_data, request, sector_count)
            }
            ScsiOp::MODE_SELECT | ScsiOp::MODE_SELECT10 => {
                self.handle_mode_select(external_data, request)
            }
            ScsiOp::VERIFY
            | ScsiOp::VERIFY12
            | ScsiOp::VERIFY16
            | ScsiOp::WRITE_VERIFY
            | ScsiOp::WRITE_VERIFY12
            | ScsiOp::WRITE_VERIFY16 => self.handle_verify_validation(request, sector_count),
            _ => {
                tracing::debug!(?op, "illegal command");
                Err(ScsiError::IllegalRequest(
                    AdditionalSenseCode::ILLEGAL_COMMAND,
                ))
            }
        }
    }

    fn validate_data_cdb(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
        sector_count: u64,
    ) -> Result<RequestParameters, ScsiError> {
        let cdb = scsi::Cdb10::read_from_prefix(&request.cdb[..]).unwrap().0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        if cdb.flags.relative_address() {
            tracing::debug!(flags = ?cdb.flags, "doesn't support relative address");
            return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
        }
        let len = cdb.transfer_blocks.get() as u64;
        let offset = cdb.logical_block.get() as u64;
        let sector_shift = self.sector_shift;
        let max = external_data.len() >> sector_shift;
        if len == 0 || len as usize > max {
            tracelimit::error_ratelimited!(len, max, "illegal block");
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::ILLEGAL_BLOCK,
            ));
        }

        if sector_count <= offset || sector_count - offset < len {
            tracelimit::error_ratelimited!(sector_count, offset, len, "illegal block");
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::ILLEGAL_BLOCK,
            ));
        }

        let fua = cdb.flags.fua();
        let tx = (len as usize) << sector_shift;
        Ok(RequestParameters { tx, offset, fua })
    }

    fn validate_data_cdb6_read_write(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
        sector_count: u64,
    ) -> Result<RequestParameters, ScsiError> {
        let cdb = scsi::Cdb6ReadWrite::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let len = cdb.transfer_blocks as u64;
        let offset = u32::from_be_bytes([
            0,
            cdb.logical_block[0],
            cdb.logical_block[1],
            cdb.logical_block[2],
        ]) as u64;
        let sector_shift = self.sector_shift;
        let max = external_data.len() >> sector_shift;
        if len == 0 || len as usize > max {
            tracelimit::error_ratelimited!(len, max, "illegal block");
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::ILLEGAL_BLOCK,
            ));
        }

        if sector_count <= offset || sector_count - offset < len {
            tracelimit::error_ratelimited!(sector_count, offset, len, "illegal block");
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::ILLEGAL_BLOCK,
            ));
        }

        let tx = (len as usize) << sector_shift;
        Ok(RequestParameters {
            tx,
            offset,
            fua: false,
        })
    }

    fn validate_data_cdb12(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
        sector_count: u64,
    ) -> Result<RequestParameters, ScsiError> {
        let cdb = scsi::Cdb12::read_from_prefix(&request.cdb[..]).unwrap().0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        if cdb.flags.relative_address() {
            tracing::debug!(flags = ?cdb.flags, "doesn't support relative address");
            return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
        }
        let len = cdb.transfer_blocks.get() as u64;
        let offset = cdb.logical_block.get() as u64;
        let max = external_data.len() >> self.sector_shift;
        if len == 0 || len as usize > max {
            tracelimit::error_ratelimited!(len, max, "illegal block");
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::ILLEGAL_BLOCK,
            ));
        }

        if sector_count <= offset || sector_count - offset < len {
            tracelimit::error_ratelimited!(sector_count, offset, len, "illegal block");
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::ILLEGAL_BLOCK,
            ));
        }

        let fua = cdb.flags.fua();
        let tx = (len as usize) << self.sector_shift;
        Ok(RequestParameters { tx, offset, fua })
    }

    fn validate_data_cdb16(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
        sector_count: u64,
    ) -> Result<RequestParameters, ScsiError> {
        let cdb = scsi::Cdb16::read_from_prefix(&request.cdb[..]).unwrap().0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let len = cdb.transfer_blocks.get() as u64;
        let offset = cdb.logical_block.get();
        let sector_shift = self.sector_shift;
        let max = external_data.len() >> sector_shift;
        if len == 0 || len as usize > max {
            tracelimit::error_ratelimited!(len, max, "illegal block");
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::ILLEGAL_BLOCK,
            ));
        }

        if sector_count <= offset || sector_count - offset < len {
            tracelimit::error_ratelimited!(sector_count, offset, len, "illegal block");
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::ILLEGAL_BLOCK,
            ));
        }

        let fua = cdb.flags.fua();
        let tx = (len as usize) << sector_shift;
        Ok(RequestParameters { tx, offset, fua })
    }

    fn validate_data_cdb16_atomic(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
        sector_count: u64,
    ) -> Result<RequestParameters, ScsiError> {
        let cdb = scsi::Cdb16Atomic::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0;
        let block_count = cdb.transfer_blocks.get() as u64;
        let offset = cdb.logical_block.get();
        let sector_shift = self.sector_shift;
        let max = external_data.len() >> sector_shift;
        let atomic_boundary = cdb.atomic_boundary.get();
        if block_count == 0 || block_count as usize > max {
            tracelimit::error_ratelimited!(block_count, max, "illegal block");
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::ILLEGAL_BLOCK,
            ));
        }

        if sector_count <= offset || sector_count - offset < block_count {
            tracelimit::error_ratelimited!(sector_count, offset, block_count, "illegal block");
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::ILLEGAL_BLOCK,
            ));
        }

        // Ensure atomicity is supported
        if self
            .scsi_parameters
            .atomic_parameters
            .maximum_atomic_transfer_length
            == 0
        {
            tracelimit::error_ratelimited!("atomic operations not supported");
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::ILLEGAL_COMMAND,
            ));
        }

        // Validate atomic-specific parameters
        // If atomic boundary is 0, make sure transfer length not larger than max atomic
        // transfer length
        if atomic_boundary == 0
            && block_count as u32
                > self
                    .scsi_parameters
                    .atomic_parameters
                    .maximum_atomic_transfer_length
        {
            tracelimit::error_ratelimited!(
                block_count,
                maximum_atomic_transfer_length = self
                    .scsi_parameters
                    .atomic_parameters
                    .maximum_atomic_transfer_length,
                "atomic boundary 0, transfer length > maximum atomic transfer length"
            );
            return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
        }

        // If atomic boundary greater than 0, make sure transfer length not larger than max atomic
        // transfer length with atomic boundary
        if atomic_boundary > 0
            && len as u32
                > self
                    .scsi_parameters
                    .atomic_parameters
                    .maximum_atomic_transfer_length_with_atomic_boundary
        {
            tracelimit::error_ratelimited!(
                atomic_boundary,
                maximum_atomic_transfer_length_with_atomic_boundary = self
                    .scsi_parameters
                    .atomic_parameters
                    .maximum_atomic_transfer_length_with_atomic_boundary,
                "atomic boundary > 0, transfer length > maximum atomic transfer length with atomic boundary"
            );
            return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
        }

        // If atomic boundary non-zero, make sure less than or equal to max atomic boundary size
        if atomic_boundary > 0
            && atomic_boundary as u32
                > self
                    .scsi_parameters
                    .atomic_parameters
                    .maximum_atomic_boundary_size
        {
            tracelimit::error_ratelimited!(
                atomic_boundary,
                maximum_atomic_boundary_size = self
                    .scsi_parameters
                    .atomic_parameters
                    .maximum_atomic_boundary_size,
                "atomic boundary > maximum atomic boundary size"
            );
            return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
        }

        // If atomic transfer length granularity non-zero, make sure that either transfer length OR
        // the non-zero atomic boundary field is a multiple of this granularity
        if self
            .scsi_parameters
            .atomic_parameters
            .atomic_transfer_length_granularity
            > 0
        {
            if len as u32
                % self
                    .scsi_parameters
                    .atomic_parameters
                    .atomic_transfer_length_granularity
                != 0
                && (atomic_boundary > 0
                    && atomic_boundary as u32
                        % self
                            .scsi_parameters
                            .atomic_parameters
                            .atomic_transfer_length_granularity
                        != 0)
            {
                tracelimit::error_ratelimited!(
                    len,
                    atomic_boundary,
                    atomic_transfer_length_granularity = self
                        .scsi_parameters
                        .atomic_parameters
                        .atomic_transfer_length_granularity,
                    "transfer length OR non-zero atomic boundary not multiple of atomic transfer length granularity"
                );
                return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
            }
        }

        let fua = cdb.flags.fua();
        let tx = (len as usize) << sector_shift;
        Ok(RequestParameters { tx, offset, fua })
    }

    fn validate_write_same(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
        sector_count: u64,
    ) -> Result<WriteSameParameters, ScsiError> {
        let op = request.scsiop();
        let mut p = match op {
            ScsiOp::WRITE_SAME => {
                let cdb = scsi::Cdb10::read_from_prefix(&request.cdb[..]).unwrap().0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
                if cdb.flags.relative_address() {
                    tracing::debug!(flags = ?cdb.flags, "doesn't support relative address");
                    return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
                }
                WriteSameParameters {
                    start_lba: cdb.logical_block.get() as u64,
                    lba_count: cdb.transfer_blocks.get() as usize,
                    fua: cdb.flags.fua(),
                    sector_size: 0,
                    tx: 0,
                }
            }
            ScsiOp::WRITE_SAME16 => {
                let cdb = scsi::Cdb16::read_from_prefix(&request.cdb[..]).unwrap().0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
                WriteSameParameters {
                    start_lba: cdb.logical_block.get(),
                    lba_count: cdb.transfer_blocks.get() as usize,
                    fua: cdb.flags.fua(),
                    sector_size: 0,
                    tx: 0,
                }
            }
            _ => unreachable!(),
        };

        if !validate_lba_range(sector_count, p.start_lba, p.lba_count.try_into().unwrap()) {
            //valiate_lba_range trace errors
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::ILLEGAL_BLOCK,
            ));
        }

        if self.disk.is_read_only() {
            return Err(ScsiError::WriteProtected);
        }

        // max length check
        p.tx = p.lba_count << self.sector_shift;
        if p.tx > VHDMP_MAX_WRITE_SAME_LENGTH_BYTES.try_into().unwrap()
            || p.tx > self.scsi_parameters.maximum_transfer_length
        {
            tracelimit::error_ratelimited!(p.tx, "transfer length too big");
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::ILLEGAL_BLOCK,
            ));
        }

        // The size of the supplied data buffer must be at least one sector.
        p.sector_size = self.sector_size.try_into().unwrap();
        let external_data_len = external_data.len();
        if p.lba_count > 0 && external_data_len < p.sector_size {
            tracelimit::error_ratelimited!(external_data_len, "provided transfer length too small");
            return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
        }

        Ok(p)
    }

    fn process_result(&self, result: Result<usize, ScsiError>, op: ScsiOp) -> ScsiResult {
        let result = result.map_err(|err| {
            match err {
                ScsiError::UnsupportedModePageCode(..)
                | ScsiError::UnsupportedServiceAction(_)
                | ScsiError::UnsupportedVpdPageCode(_) => tracing::debug!(disk = ?self.scsi_parameters.disk_id, error = err.as_error(), ?op, "scsi_error"),
                | ScsiError::IllegalRequest(_) => tracing::debug!(disk = ?self.scsi_parameters.disk_id, error = err.as_error(), ?op, "scsi_error"),
                _ => tracelimit::warn_ratelimited!(disk = ?self.scsi_parameters.disk_id, error = err.as_error(), ?op, "scsi_error"),
            }
            err
        });

        let result = match result {
            Ok(tx) => ScsiResult {
                scsi_status: ScsiStatus::GOOD,
                srb_status: SrbStatus::SUCCESS,
                tx,
                sense_data: None,
            },
            Err(err) => {
                match err {
                    ScsiError::MemoryAccess(_)
                    | ScsiError::UnsupportedModePageCode(..)
                    | ScsiError::UnsupportedServiceAction(_)
                    | ScsiError::UnsupportedVpdPageCode(_)
                    | ScsiError::SrbError
                    | ScsiError::Disk(DiskError::InvalidInput)
                    | ScsiError::Disk(DiskError::MemoryAccess(_)) => ScsiResult {
                        scsi_status: ScsiStatus::CHECK_CONDITION,
                        srb_status: SrbStatus::INVALID_REQUEST,
                        tx: 0,
                        sense_data: Some(illegal_request_sense(AdditionalSenseCode::INVALID_CDB)),
                    },
                    ScsiError::IllegalRequest(sense_code) => ScsiResult {
                        scsi_status: ScsiStatus::CHECK_CONDITION,
                        srb_status: SrbStatus::INVALID_REQUEST,
                        tx: 0,
                        sense_data: Some(illegal_request_sense(sense_code)),
                    },
                    ScsiError::DataOverrun => ScsiResult {
                        scsi_status: ScsiStatus::CHECK_CONDITION,
                        srb_status: SrbStatus::DATA_OVERRUN,
                        tx: 0,
                        sense_data: Some(illegal_request_sense(AdditionalSenseCode::INVALID_CDB)),
                    },
                    ScsiError::UnitAttention => ScsiResult {
                        scsi_status: ScsiStatus::CHECK_CONDITION,
                        srb_status: SrbStatus::ERROR,
                        tx: 0,
                        sense_data: Some(scsi::SenseData::new(
                            SenseKey::UNIT_ATTENTION,
                            AdditionalSenseCode::PARAMETERS_CHANGED,
                            scsi::SCSI_SENSEQ_CAPACITY_DATA_CHANGED,
                        )),
                    },
                    ScsiError::WriteProtected | ScsiError::Disk(DiskError::ReadOnly) => {
                        ScsiResult {
                            scsi_status: ScsiStatus::CHECK_CONDITION,
                            srb_status: SrbStatus::ERROR,
                            tx: 0,
                            sense_data: Some(scsi::SenseData::new(
                                SenseKey::DATA_PROTECT,
                                AdditionalSenseCode::WRITE_PROTECT,
                                0,
                            )),
                        }
                    }
                    ScsiError::Disk(err) => {
                        match err {
                            DiskError::AbortDueToPreemptAndAbort => ScsiResult {
                                scsi_status: ScsiStatus::TASK_ABORTED,
                                srb_status: SrbStatus::ABORTED,
                                tx: 0,
                                sense_data: Some(scsi::SenseData::new(
                                    SenseKey::ABORTED_COMMAND,
                                    AdditionalSenseCode::NO_SENSE,
                                    0,
                                )),
                            },
                            DiskError::IllegalBlock => ScsiResult {
                                scsi_status: ScsiStatus::CHECK_CONDITION,
                                srb_status: SrbStatus::ERROR,
                                tx: 0,
                                sense_data: Some(scsi::SenseData::new(
                                    SenseKey::ILLEGAL_REQUEST,
                                    AdditionalSenseCode::ILLEGAL_BLOCK,
                                    0,
                                )),
                            },
                            DiskError::Io(_) => ScsiResult {
                                scsi_status: ScsiStatus::CHECK_CONDITION,
                                srb_status: SrbStatus::ERROR,
                                tx: 0,
                                sense_data: Some(scsi::SenseData::new(
                                    SenseKey::MEDIUM_ERROR,
                                    AdditionalSenseCode::NO_SENSE,
                                    0,
                                )),
                            },
                            DiskError::MediumError(_, details) => {
                                let (sense_code, qualifier) = match details {
                                    disk_backend::MediumErrorDetails::ApplicationTagCheckFailed => {
                                        (
                                            AdditionalSenseCode::UNRECOVERED_ERROR,
                                            scsi::SCSI_SENSEQ_LOGICAL_BLOCK_TAG_CHECK_FAILED,
                                        )
                                    }
                                    disk_backend::MediumErrorDetails::GuardCheckFailed => (
                                        AdditionalSenseCode::CRC_OR_ECC_ERROR,
                                        scsi::SCSI_SENSEQ_LOGICAL_BLOCK_GUARD_CHECK_FAILED,
                                    ),
                                    disk_backend::MediumErrorDetails::ReferenceTagCheckFailed => (
                                        AdditionalSenseCode::CRC_OR_ECC_ERROR,
                                        scsi::SCSI_SENSEQ_LOGICAL_BLOCK_REF_TAG_CHECK_FAILED,
                                    ),
                                    disk_backend::MediumErrorDetails::UnrecoveredReadError => {
                                        (AdditionalSenseCode::UNRECOVERED_ERROR, 0)
                                    }
                                    disk_backend::MediumErrorDetails::WriteFault => {
                                        (AdditionalSenseCode::WRITE, 0)
                                    }
                                };
                                ScsiResult {
                                    scsi_status: ScsiStatus::CHECK_CONDITION,
                                    srb_status: SrbStatus::ERROR,
                                    tx: 0,
                                    sense_data: Some(scsi::SenseData::new(
                                        SenseKey::MEDIUM_ERROR,
                                        sense_code,
                                        qualifier,
                                    )),
                                }
                            }
                            DiskError::ReservationConflict => ScsiResult {
                                scsi_status: ScsiStatus::RESERVATION_CONFLICT,
                                srb_status: SrbStatus::ERROR,
                                tx: 0,
                                sense_data: None,
                            },
                            DiskError::UnsupportedEject => ScsiResult {
                                scsi_status: ScsiStatus::CHECK_CONDITION,
                                srb_status: SrbStatus::INVALID_REQUEST,
                                tx: 0,
                                sense_data: Some(illegal_request_sense(
                                    AdditionalSenseCode::ILLEGAL_COMMAND,
                                )),
                            },
                            DiskError::InvalidInput
                            | DiskError::MemoryAccess(_)
                            | DiskError::ReadOnly => unreachable!(), //handled above
                        }
                    }
                }
            }
        };

        self.sense_data.set(result.sense_data.as_ref());
        if op == ScsiOp::PERSISTENT_RESERVE_OUT && result.scsi_status != ScsiStatus::GOOD {
            tracing::warn!(scsi_result = ?result, "PERSISTENT_RESERVE_OUT failed.");
        } else {
            tracing::trace!(scsi_result = ?result, ?op, "process_result completed.");
        }

        result
    }

    /// Gets the current sector count from the underlying disk.
    ///
    /// If the sector count has changed since the last call, returns an error so
    /// that the caller can propagate unit attention to the guest.
    ///
    /// For `INQUIRY`, returns the new sector count without error but does not
    /// update the last observed one.
    fn get_and_update_sector_count(&self, op: ScsiOp) -> Result<u64, u64> {
        let current = self.last_sector_count.load(Ordering::Relaxed);
        let sector_count = self.disk.sector_count();
        // Don't process sector count updates during inquiry (but do report the new sector size).
        if sector_count == current || op == ScsiOp::INQUIRY {
            return Ok(sector_count);
        }
        tracing::info!(
            sector_count,
            old_sector_count = current,
            "updating sector count"
        );
        if self
            .last_sector_count
            .compare_exchange(current, sector_count, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            // Another request already handled the unit attention.
            return Ok(sector_count);
        }
        Err(sector_count)
    }
}

impl SimpleScsiDisk {
    async fn handle_data_cdb(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
        sector_count: u64,
    ) -> Result<usize, ScsiError> {
        let op = request.scsiop();
        let is_read;
        let p = match op {
            ScsiOp::READ | ScsiOp::WRITE => {
                is_read = op == ScsiOp::READ;
                self.validate_data_cdb(external_data, request, sector_count)?
            }
            ScsiOp::READ6 | ScsiOp::WRITE6 => {
                is_read = op == ScsiOp::READ6;
                self.validate_data_cdb6_read_write(external_data, request, sector_count)?
            }
            ScsiOp::READ12 | ScsiOp::WRITE12 => {
                is_read = op == ScsiOp::READ12;
                self.validate_data_cdb12(external_data, request, sector_count)?
            }
            ScsiOp::READ16 | ScsiOp::WRITE16 => {
                is_read = op == ScsiOp::READ16;
                self.validate_data_cdb16(external_data, request, sector_count)?
            }
            ScsiOp::WRITE_ATOMIC16 => {
                is_read = false;
                self.validate_data_cdb16_atomic(external_data, request, sector_count)?
            }
            _ => unreachable!(),
        };

        // Note that `p.tx` is validated above to be in range.
        let external_data = external_data.subrange(0, p.tx);

        Ok(if is_read {
            self.disk
                .read_vectored(&external_data, p.offset)
                .await
                .map_err(ScsiError::Disk)?;

            p.tx
        } else {
            if self.disk.is_read_only() {
                return Err(ScsiError::WriteProtected);
            }

            self.disk
                .write_vectored(&external_data, p.offset, p.fua)
                .await
                .map_err(ScsiError::Disk)?;

            p.tx
        })
    }

    async fn handle_synchronize_cache(&self) -> Result<usize, ScsiError> {
        self.disk.sync_cache().await.map_err(ScsiError::Disk)?;
        Ok(0)
    }

    async fn handle_write_same(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
        sector_count: u64,
    ) -> Result<usize, ScsiError> {
        let p = self.validate_write_same(external_data, request, sector_count)?;
        if p.tx > 0 {
            // Note that `p.sector_size` is validated above to be in range.
            let external_data = external_data.subrange(0, p.sector_size);
            // TODO: pass this request through to the disk rather than looping like this.
            for offset in p.start_lba..p.start_lba + (p.lba_count as u64) {
                self.disk
                    .write_vectored(&external_data, offset, p.fua)
                    .await
                    .map_err(ScsiError::Disk)?;
            }
        }

        Ok(p.tx)
    }

    async fn handle_start_stop(&self, request: &Request) -> Result<usize, ScsiError> {
        let cdb = scsi::StartStop::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        if cdb.immediate & scsi::IMMEDIATE_BIT != 0 {
            tracing::debug!("immediate bit is not supported");
            return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
        }

        if cdb.flag.start() {
            return Ok(0);
        };

        self.disk.sync_cache().await.map_err(ScsiError::Disk)?;
        Ok(0)
    }
}

impl AsyncScsiDisk for SimpleScsiDisk {
    fn execute_scsi<'a>(
        &'a self,
        external_data: &'a RequestBuffers<'a>,
        request: &'a Request,
    ) -> StackFuture<'a, ScsiResult, { ASYNC_SCSI_DISK_STACK_SIZE }> {
        StackFuture::from(async move {
            let op = request.scsiop();

            let sector_count = match self.get_and_update_sector_count(op) {
                Ok(c) => c,
                Err(_) => {
                    // The sector count has changed. Report unit attention.
                    let result = match op {
                        ScsiOp::REQUEST_SENSE => {
                            self.handle_request_sense(external_data, request, true)
                        }
                        _ => Err(ScsiError::UnitAttention),
                    };
                    return self.process_result(result, op);
                }
            };

            let result = match op {
                ScsiOp::WRITE
                | ScsiOp::WRITE6
                | ScsiOp::WRITE12
                | ScsiOp::WRITE16
                | ScsiOp::WRITE_ATOMIC16
                | ScsiOp::READ
                | ScsiOp::READ6
                | ScsiOp::READ12
                | ScsiOp::READ16 => {
                    self.handle_data_cdb(external_data, request, sector_count)
                        .instrument(tracing::trace_span!("handle_data_cdb_async", ?op,))
                        .await
                }
                ScsiOp::WRITE_SAME | ScsiOp::WRITE_SAME16 => {
                    self.handle_write_same(external_data, request, sector_count)
                        .instrument(tracing::trace_span!("handle_write_same_async"))
                        .await
                }
                ScsiOp::SYNCHRONIZE_CACHE | ScsiOp::SYNCHRONIZE_CACHE16 => {
                    self.handle_synchronize_cache()
                        .instrument(tracing::trace_span!("handle_synchronize_cache_async", ?op,))
                        .await
                }
                ScsiOp::START_STOP_UNIT => {
                    self.handle_start_stop(request)
                        .instrument(tracing::trace_span!("handle_start_stop_async",))
                        .await
                }
                ScsiOp::UNMAP => {
                    self.handle_unmap(external_data, request, sector_count)
                        .instrument(tracing::debug_span!("handle_unmap_async"))
                        .await
                }
                ScsiOp::PERSISTENT_RESERVE_IN | ScsiOp::PERSISTENT_RESERVE_OUT => {
                    self.handle_persistent_reserve(external_data, request)
                        .instrument(tracing::trace_span!("handle_persistent_reserve_async", ?op,))
                        .await
                }
                _ => {
                    let _span = tracing::trace_span!("handle_control_cdb", ?op,).entered();
                    self.handle_control_cdb(external_data, request, sector_count)
                }
            };

            self.process_result(result, op)
        })
    }
}

impl Inspect for SimpleScsiDisk {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        resp.binary("disk_id", self.scsi_parameters.disk_id)
            .field("logical_sector_size", self.sector_size)
            .field(
                "physical_sector_size",
                1usize << self.sector_shift << self.physical_extra_shift,
            )
            .field(
                "sector_count",
                self.last_sector_count.load(Ordering::Relaxed),
            )
            .field("scsi_parameters", &self.scsi_parameters)
            .field("pr", self.support_pr)
            .field("backend", &self.disk);
    }
}

#[derive(Default, Debug)]
struct SenseDataSlot {
    is_valid: AtomicBool,
    data: Mutex<Option<scsi::SenseData>>,
}

impl SenseDataSlot {
    /// Updates sense data.
    fn set(&self, sense_data: Option<&scsi::SenseData>) {
        match sense_data {
            None => {
                // Only clear sense data if it is set to avoid taking the cache
                // line exclusive in the common case.
                //
                // Access with relaxed ordering because sense data state is not
                // well defined if there are multiple concurrent IOs anyway.
                if self.is_valid.load(Ordering::Relaxed) {
                    self.is_valid.store(false, Ordering::Relaxed)
                }
            }
            Some(sense_data) => {
                *self.data.lock() = Some(*sense_data);
                self.is_valid.store(true, Ordering::Release);
            }
        }
    }

    /// Gets sense data without clearing it.
    fn get(&self) -> Option<scsi::SenseData> {
        if self.is_valid.load(Ordering::Relaxed) {
            // Note that this might still be None due to race conditions with
            // multiple concurrent IOs.
            *self.data.lock()
        } else {
            None
        }
    }

    /// Gets and clears sense data.
    pub(crate) fn take(&self) -> Option<scsi::SenseData> {
        if self.is_valid.swap(false, Ordering::Acquire) {
            // Note that this might still be None due to race conditions with
            // multiple concurrent IOs.
            self.data.lock().take()
        } else {
            None
        }
    }
}
