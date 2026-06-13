// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVMe NVM namespace implementation.

mod reservations;

use crate::error::CommandResult;
use crate::error::NvmeError;
use crate::prp::PrpRange;
use crate::spec;
use crate::spec::nvm;
use disk_backend::Disk;
use guestmem::GuestMemory;
use inspect::Inspect;
use scsi_buffers::RequestBuffers;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// An NVMe namespace built on top of a [`Disk`].
#[derive(Inspect)]
pub struct Namespace {
    disk: Disk,
    nsid: u32,
    mem: GuestMemory,
    block_shift: u32,
    pr: bool,
}

impl Namespace {
    pub fn new(mem: GuestMemory, nsid: u32, disk: Disk) -> Self {
        Self {
            block_shift: disk.sector_size().trailing_zeros(),
            pr: disk.pr().is_some(),
            mem,
            disk,
            nsid,
        }
    }

    pub fn identify(&self, buf: &mut [u8]) {
        let id = nvm::IdentifyNamespace::mut_from_prefix(buf).unwrap().0; // TODO: zerocopy: from-prefix (mut_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let size = self.disk.sector_count();

        let rescap = if let Some(pr) = self.disk.pr() {
            let caps = pr.capabilities();
            nvm::ReservationCapabilities::new()
                .with_write_exclusive(caps.write_exclusive)
                .with_exclusive_access(caps.exclusive_access)
                .with_write_exclusive_registrants_only(caps.write_exclusive_registrants_only)
                .with_exclusive_access_registrants_only(caps.exclusive_access_registrants_only)
                .with_write_exclusive_all_registrants(caps.write_exclusive_all_registrants)
                .with_exclusive_access_all_registrants(caps.exclusive_access_all_registrants)
        } else {
            nvm::ReservationCapabilities::new()
        };

        *id = nvm::IdentifyNamespace {
            nsze: size,
            ncap: size,
            nuse: size,
            nlbaf: 0,
            flbas: nvm::Flbas::new().with_low_index(0),
            rescap,
            ..FromZeros::new_zeroed()
        };
        id.lbaf[0] = nvm::Lbaf::new().with_lbads(self.block_shift as u8);
    }

    pub fn namespace_id_descriptor(&self, buf: &mut [u8]) {
        let id = nvm::NamespaceIdentificationDescriptor::mut_from_prefix(buf)
            .unwrap()
            .0; // TODO: zerocopy: from-prefix (mut_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let mut nid = [0u8; 0x10];
        if let Some(guid) = self.disk.disk_id() {
            nid = guid;
        }
        *id = nvm::NamespaceIdentificationDescriptor {
            nidt: nvm::NamespaceIdentifierType::NSGUID.0,
            nidl: size_of_val(&nid) as u8,
            rsvd: [0, 0],
            nid,
        };
    }

    pub async fn get_feature(&self, command: &spec::Command) -> Result<CommandResult, NvmeError> {
        let cdw10: spec::Cdw10GetFeatures = command.cdw10.into();
        let mut dw = [0; 2];

        // Note that we don't support non-zero cdw10.sel, since ONCS.save == 0.
        match spec::Feature(cdw10.fid()) {
            spec::Feature::NVM_RESERVATION_PERSISTENCE if self.pr => {
                dw[0] = self
                    .get_reservation_persistence(self.disk.pr().unwrap())
                    .await?
                    .into();
            }
            feature => {
                tracelimit::warn_ratelimited!(nsid = self.nsid, ?feature, "unsupported feature");
                return Err(spec::Status::INVALID_FIELD_IN_COMMAND.into());
            }
        }
        Ok(CommandResult::new(spec::Status::SUCCESS, dw))
    }

    /// Waits for the namespace identify result to change.
    ///
    /// Returns an opaque token to use for the next wait.
    pub async fn wait_change(&self, token: Option<u64>) -> u64 {
        // Use the sector count as the token, since that's the only thing that
        // can currently change.
        let sector_count = token.unwrap_or_else(|| self.disk.sector_count());
        self.disk.wait_resize(sector_count).await
    }

    pub async fn nvm_command(
        &self,
        max_data_transfer_size: usize,
        command: &spec::Command,
    ) -> Result<CommandResult, NvmeError> {
        let opcode = nvm::NvmOpcode(command.cdw0.opcode());
        tracing::trace!(nsid = self.nsid, ?opcode, ?command, "nvm command");

        match opcode {
            nvm::NvmOpcode::READ => {
                let cdw10 = nvm::Cdw10ReadWrite::from(command.cdw10);
                let cdw11 = nvm::Cdw11ReadWrite::from(command.cdw11);
                let cdw12 = nvm::Cdw12ReadWrite::from(command.cdw12);
                let lba = cdw10.sbla_low() as u64 | ((cdw11.sbla_high() as u64) << 32);
                let count = cdw12.nlb_z() as usize + 1;
                let byte_count = count << self.block_shift;
                if byte_count > max_data_transfer_size {
                    return Err(spec::Status::INVALID_FIELD_IN_COMMAND.into());
                }
                let range = PrpRange::parse(&self.mem, byte_count, command.dptr)?;

                let disk_sector_count = self.disk.sector_count();
                if disk_sector_count < lba || disk_sector_count - lba < count as u64 {
                    return Err(spec::Status::LBA_OUT_OF_RANGE.into());
                }

                tracing::trace!(nsid = self.nsid, lba, count, byte_count, "read");

                let buffers = RequestBuffers::new(&self.mem, range.range(), true);
                self.disk
                    .read_vectored(&buffers, lba)
                    .await
                    .map_err(map_disk_error)?;
            }
            nvm::NvmOpcode::WRITE => {
                let cdw10 = nvm::Cdw10ReadWrite::from(command.cdw10);
                let cdw11 = nvm::Cdw11ReadWrite::from(command.cdw11);
                let cdw12 = nvm::Cdw12ReadWrite::from(command.cdw12);
                let lba = cdw10.sbla_low() as u64 | ((cdw11.sbla_high() as u64) << 32);
                let count = cdw12.nlb_z() as usize + 1;
                let byte_count = count << self.block_shift;
                if byte_count > max_data_transfer_size {
                    return Err(spec::Status::INVALID_FIELD_IN_COMMAND.into());
                }
                let range = PrpRange::parse(&self.mem, byte_count, command.dptr)?;

                let disk_sector_count = self.disk.sector_count();
                if disk_sector_count < lba || disk_sector_count - lba < count as u64 {
                    return Err(spec::Status::LBA_OUT_OF_RANGE.into());
                }

                tracing::trace!(nsid = self.nsid, lba, count, byte_count, "write");

                let buffers = RequestBuffers::new(&self.mem, range.range(), false);
                self.disk
                    .write_vectored(&buffers, lba, cdw12.fua())
                    .await
                    .map_err(map_disk_error)?;
            }
            nvm::NvmOpcode::FLUSH => {
                tracing::debug!(nsid = self.nsid, "flush");
                if !self.disk.is_read_only() {
                    self.disk.sync_cache().await.map_err(map_disk_error)?;
                }
            }
            nvm::NvmOpcode::DSM => {
                let cdw10 = nvm::Cdw10Dsm::from(command.cdw10);
                let cdw11 = nvm::Cdw11Dsm::from(command.cdw11);
                // TODO: zerocopy: manual: review carefully! (https://github.com/microsoft/openvmm/issues/759)
                let mut dsm_ranges =
                    <[nvm::DsmRange]>::new_box_zeroed_with_elems(cdw10.nr_z() as usize + 1)
                        .unwrap();
                let prp =
                    PrpRange::parse(&self.mem, size_of_val(dsm_ranges.as_ref()), command.dptr)?;
                prp.read(&self.mem, dsm_ranges.as_mut_bytes())?;
                tracing::debug!(nsid = self.nsid, ?cdw11, ?dsm_ranges, "dsm");
                if cdw11.ad() {
                    for range in dsm_ranges.as_ref() {
                        self.disk
                            .unmap(range.starting_lba, range.lba_count.into(), false)
                            .await
                            .map_err(map_disk_error)?;
                    }
                }
            }
            nvm::NvmOpcode::RESERVATION_REGISTER if self.pr => {
                self.reservation_register(self.disk.pr().unwrap(), command)
                    .await?
            }
            nvm::NvmOpcode::RESERVATION_REPORT if self.pr => {
                self.reservation_report(self.disk.pr().unwrap(), command)
                    .await?
            }
            nvm::NvmOpcode::RESERVATION_ACQUIRE if self.pr => {
                self.reservation_acquire(self.disk.pr().unwrap(), command)
                    .await?
            }
            nvm::NvmOpcode::RESERVATION_RELEASE if self.pr => {
                self.reservation_release(self.disk.pr().unwrap(), command)
                    .await?
            }
            opcode => {
                tracelimit::warn_ratelimited!(nsid = self.nsid, ?opcode, "unsupported nvm opcode");
                return Err(spec::Status::INVALID_COMMAND_OPCODE.into());
            }
        }
        Ok(Default::default())
    }
}

fn map_disk_error(err: disk_backend::DiskError) -> NvmeError {
    match err {
        disk_backend::DiskError::ReservationConflict => spec::Status::RESERVATION_CONFLICT.into(),
        disk_backend::DiskError::MemoryAccess(err) => {
            NvmeError::new(spec::Status::DATA_TRANSFER_ERROR, err)
        }
        disk_backend::DiskError::AbortDueToPreemptAndAbort => {
            NvmeError::new(spec::Status::COMMAND_ABORTED_DUE_TO_PREEMPT_AND_ABORT, err)
        }
        disk_backend::DiskError::IllegalBlock => spec::Status::LBA_OUT_OF_RANGE.into(),
        disk_backend::DiskError::InvalidInput => spec::Status::INVALID_FIELD_IN_COMMAND.into(),
        disk_backend::DiskError::Io(err) => NvmeError::new(spec::Status::DATA_TRANSFER_ERROR, err),
        disk_backend::DiskError::MediumError(_, details) => match details {
            disk_backend::MediumErrorDetails::ApplicationTagCheckFailed => {
                spec::Status::MEDIA_END_TO_END_APPLICATION_TAG_CHECK_ERROR.into()
            }
            disk_backend::MediumErrorDetails::GuardCheckFailed => {
                spec::Status::MEDIA_END_TO_END_GUARD_CHECK_ERROR.into()
            }
            disk_backend::MediumErrorDetails::ReferenceTagCheckFailed => {
                spec::Status::MEDIA_END_TO_END_REFERENCE_TAG_CHECK_ERROR.into()
            }
            disk_backend::MediumErrorDetails::UnrecoveredReadError => {
                spec::Status::MEDIA_UNRECOVERED_READ_ERROR.into()
            }
            disk_backend::MediumErrorDetails::WriteFault => spec::Status::MEDIA_WRITE_FAULT.into(),
        },
        disk_backend::DiskError::ReadOnly => {
            spec::Status::ATTEMPTED_WRITE_TO_READ_ONLY_RANGE.into()
        }
        disk_backend::DiskError::UnsupportedEject => spec::Status::INVALID_COMMAND_OPCODE.into(),
    }
}
