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
    /// Stable backing-store identity, when the disk supplies one.
    ///
    /// A missing or all-zero disk ID is not replaced with a controller/NSID
    /// derived value: those identify an attachment slot, not the backing
    /// namespace, and can be reused for a different disk.
    nguid: Option<[u8; 16]>,
}

impl Namespace {
    pub fn new(mem: GuestMemory, nsid: u32, disk: Disk) -> Self {
        let nguid = disk.disk_id().filter(|id| *id != [0; 16]);
        Self {
            block_shift: disk.sector_size().trailing_zeros(),
            pr: disk.pr().is_some(),
            mem,
            disk,
            nsid,
            nguid,
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
            // Populate the inline field only when the backing store supplies
            // a suitable identity. EUI64 remains zero.
            nguid: self.nguid.unwrap_or_default(),
            nsattr: nvm::Nsattr::new().with_write_protected(self.disk.is_read_only()),
            ..FromZeros::new_zeroed()
        };
        id.lbaf[0] = nvm::Lbaf::new().with_lbads(self.block_shift as u8);
    }

    pub fn namespace_id_descriptor(&self, buf: &mut [u8]) {
        buf.fill(0);
        let Some(nguid) = self.nguid else {
            return;
        };
        let id = nvm::NamespaceIdentificationDescriptor::mut_from_prefix(buf)
            .unwrap()
            .0; // TODO: zerocopy: from-prefix (mut_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        *id = nvm::NamespaceIdentificationDescriptor {
            nidt: nvm::NamespaceIdentifierType::NSGUID.0,
            nidl: size_of_val(&nguid) as u8,
            rsvd: [0, 0],
            nid: nguid,
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

        // Identify Controller reports no fused operations (FUSES is 0) and no
        // SGL support (SGLS is 0), so a command that sets FUSE or asks for
        // SGLs (PSDT other than 00b) is refused.
        if command.cdw0.fuse() != 0 || command.cdw0.psdt() != 0 {
            return Err(NvmeError::do_not_retry(
                spec::Status::INVALID_FIELD_IN_COMMAND,
            ));
        }

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

                if self.disk.is_read_only() {
                    return Err(spec::Status::ATTEMPTED_WRITE_TO_READ_ONLY_RANGE.into());
                }

                let buffers = RequestBuffers::new(&self.mem, range.range(), false);
                self.disk
                    .write_vectored(&buffers, lba, cdw12.fua())
                    .await
                    .map_err(map_disk_error)?;
            }
            nvm::NvmOpcode::FLUSH => {
                tracing::trace!(nsid = self.nsid, "flush");
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
                tracing::trace!(nsid = self.nsid, ?cdw11, ?dsm_ranges, "dsm");
                if cdw11.ad() {
                    if self.disk.is_read_only() {
                        return Err(spec::Status::ATTEMPTED_WRITE_TO_READ_ONLY_RANGE.into());
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use disk_backend::Disk;
    use disk_backend::DiskError;
    use disk_backend::DiskIo;
    use disk_backend::UnmapBehavior;
    use scsi_buffers::RequestBuffers;

    /// Minimal `DiskIo` shim used to feed a known `disk_id` (or `None`)
    /// into `Namespace::identify` for testing. None of the IO methods are
    /// expected to be called from `identify`; they panic if they ever are.
    #[derive(Inspect)]
    #[inspect(skip)]
    struct TestDisk {
        disk_id: Option<[u8; 16]>,
    }

    impl DiskIo for TestDisk {
        fn disk_type(&self) -> &str {
            "test"
        }
        fn sector_count(&self) -> u64 {
            256
        }
        fn sector_size(&self) -> u32 {
            512
        }
        fn disk_id(&self) -> Option<[u8; 16]> {
            self.disk_id
        }
        fn physical_sector_size(&self) -> u32 {
            512
        }
        fn is_fua_respected(&self) -> bool {
            false
        }
        fn is_read_only(&self) -> bool {
            false
        }
        fn unmap_behavior(&self) -> UnmapBehavior {
            UnmapBehavior::Ignored
        }
        async fn unmap(&self, _: u64, _: u64, _: bool) -> Result<(), DiskError> {
            unreachable!("Namespace::identify must not perform IO")
        }
        async fn read_vectored(&self, _: &RequestBuffers<'_>, _: u64) -> Result<(), DiskError> {
            unreachable!("Namespace::identify must not perform IO")
        }
        async fn write_vectored(
            &self,
            _: &RequestBuffers<'_>,
            _: u64,
            _: bool,
        ) -> Result<(), DiskError> {
            unreachable!("Namespace::identify must not perform IO")
        }
        async fn sync_cache(&self) -> Result<(), DiskError> {
            unreachable!("Namespace::identify must not perform IO")
        }
    }

    /// Byte offsets in the Identify Namespace data structure (NVMe Base
    /// Specification Figure 312 / NVM Command Set Specification Figure 97).
    const NGUID_OFFSET: usize = 104;
    const NGUID_LEN: usize = 16;
    const EUI64_OFFSET: usize = 120;
    const EUI64_LEN: usize = 8;

    /// Convenience: build a `Namespace` with a `TestDisk` backing, run
    /// `Namespace::identify`, and return the 4 KiB response buffer.
    fn identify_buf_for(nsid: u32, disk_id: Option<[u8; 16]>) -> Vec<u8> {
        let disk = Disk::new(TestDisk { disk_id }).unwrap();
        let mem = GuestMemory::empty();
        let ns = Namespace::new(mem, nsid, disk);
        let mut buf = vec![0u8; 4096];
        ns.identify(&mut buf);
        buf
    }

    /// Convenience: build a `Namespace` with a `TestDisk` backing, run
    /// `Namespace::namespace_id_descriptor`, and return the 4 KiB
    /// response buffer.
    fn descriptor_buf_for(nsid: u32, disk_id: Option<[u8; 16]>) -> Vec<u8> {
        let disk = Disk::new(TestDisk { disk_id }).unwrap();
        let mem = GuestMemory::empty();
        let ns = Namespace::new(mem, nsid, disk);
        let mut buf = vec![0u8; 4096];
        ns.namespace_id_descriptor(&mut buf);
        buf
    }

    /// When the backing disk reports a 16-byte identifier, the inline
    /// NGUID field of the Identify Namespace response (offset 104..120)
    /// must contain those bytes verbatim. EUI64 (offset 120..128) is
    /// independent and must remain zero -- there is no general way to
    /// derive a unique 8-byte EUI from the backing disk.
    #[test]
    fn test_identify_namespace_populates_inline_nguid_from_disk_id() {
        let want: [u8; 16] = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10,
        ];
        let buf = identify_buf_for(1, Some(want));
        assert_eq!(
            &buf[NGUID_OFFSET..NGUID_OFFSET + NGUID_LEN],
            &want,
            "inline NGUID must echo the disk_id bytes when the backing disk supplies one"
        );
        assert_eq!(
            &buf[EUI64_OFFSET..EUI64_OFFSET + EUI64_LEN],
            &[0u8; EUI64_LEN],
            "EUI64 must remain zero -- there is no defined derivation"
        );
    }

    /// When the backing disk reports no identifier, the inline NGUID and EUI64
    /// fields remain zero.
    #[test]
    fn test_identify_namespace_leaves_nguid_zero_when_disk_has_no_id() {
        let buf = identify_buf_for(1, None);
        assert_eq!(
            &buf[NGUID_OFFSET..NGUID_OFFSET + NGUID_LEN],
            &[0u8; NGUID_LEN]
        );
        assert_eq!(
            &buf[EUI64_OFFSET..EUI64_OFFSET + EUI64_LEN],
            &[0u8; EUI64_LEN]
        );
    }

    /// A pathological all-zero backing ID is treated as absent.
    #[test]
    fn test_identify_namespace_treats_zero_disk_id_as_absent() {
        let buf = identify_buf_for(5, Some([0u8; 16]));
        assert_eq!(
            &buf[NGUID_OFFSET..NGUID_OFFSET + NGUID_LEN],
            &[0u8; NGUID_LEN]
        );
    }

    /// The NIDT=NSGUID (NGUID in the NVMe spec) descriptor returned by CNS=03h
    /// must match the backing identity copied into the inline NGUID.
    #[test]
    fn test_namespace_id_descriptor_matches_inline_nguid_with_disk_id() {
        let want: [u8; 16] = [
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
            0xff, 0x00,
        ];
        let inline = identify_buf_for(1, Some(want));
        let desc = descriptor_buf_for(1, Some(want));
        // Descriptor layout: byte 0 = NIDT, byte 1 = NIDL, bytes 2-3
        // reserved, bytes 4..20 = NID.
        assert_eq!(desc[0], nvm::NamespaceIdentifierType::NSGUID.0);
        assert_eq!(desc[1] as usize, NGUID_LEN);
        assert_eq!(&desc[4..4 + NGUID_LEN], &want);
        assert_eq!(
            &desc[4..4 + NGUID_LEN],
            &inline[NGUID_OFFSET..NGUID_OFFSET + NGUID_LEN]
        );
    }

    /// Without a backing identity, CNS=03h contains no descriptor.
    #[test]
    fn test_namespace_id_descriptor_is_empty_without_disk_id() {
        let desc = descriptor_buf_for(7, None);
        assert!(desc.iter().all(|byte| *byte == 0));
    }

    use disklayer_ram::ram_disk;
    use pal_async::async_test;
    use zerocopy::FromZeros;
    use zerocopy::IntoBytes;

    const DATA_GPA: u64 = 0x1000;
    const SECTOR_SIZE: usize = 512;

    fn read_only_namespace() -> (GuestMemory, Namespace) {
        let mem = GuestMemory::allocate(0x2000);
        let disk = ram_disk(1 << 20, true).unwrap();
        (mem.clone(), Namespace::new(mem, 1, disk))
    }

    /// Builds a command for `opcode` with its data buffer at `DATA_GPA`.
    fn command(opcode: nvm::NvmOpcode) -> spec::Command {
        let mut command = spec::Command::new_zeroed();
        command.cdw0.set_opcode(opcode.0);
        command.nsid = 1;
        command.dptr[0] = DATA_GPA;
        command
    }

    async fn status(namespace: &Namespace, command: &spec::Command) -> spec::Status {
        match namespace.nvm_command(0x1000, command).await {
            Ok(result) => result.status,
            Err(err) => CommandResult::from(err).status,
        }
    }

    #[async_test]
    async fn write_to_read_only_namespace_fails() {
        let (mem, namespace) = read_only_namespace();

        mem.fill_at(DATA_GPA, 0xa5, SECTOR_SIZE).unwrap();
        assert_eq!(
            status(&namespace, &command(nvm::NvmOpcode::WRITE)).await,
            spec::Status::ATTEMPTED_WRITE_TO_READ_ONLY_RANGE
        );

        // A read still succeeds, and returns the original (zero) contents.
        mem.fill_at(DATA_GPA, 0xff, SECTOR_SIZE).unwrap();
        assert_eq!(
            status(&namespace, &command(nvm::NvmOpcode::READ)).await,
            spec::Status::SUCCESS
        );
        let mut data = [0xff; SECTOR_SIZE];
        mem.read_at(DATA_GPA, &mut data).unwrap();
        assert_eq!(data, [0; SECTOR_SIZE]);
    }

    #[test]
    fn identify_reports_read_only_as_write_protected() {
        for read_only in [false, true] {
            let mem = GuestMemory::allocate(0x1000);
            let namespace = Namespace::new(mem, 1, ram_disk(1 << 20, read_only).unwrap());
            let mut id = nvm::IdentifyNamespace::new_zeroed();
            namespace.identify(id.as_mut_bytes());
            assert_eq!(
                id.nsattr.write_protected(),
                read_only,
                "read_only = {read_only}"
            );
        }
    }

    #[async_test]
    async fn deallocate_on_read_only_namespace_fails() {
        let (mem, namespace) = read_only_namespace();
        mem.write_plain(
            DATA_GPA,
            &nvm::DsmRange {
                context_attributes: 0,
                lba_count: 1,
                starting_lba: 0,
            },
        )
        .unwrap();

        // Without the deallocate attribute the command does not modify the
        // namespace and succeeds.
        let mut dsm = command(nvm::NvmOpcode::DSM);
        assert_eq!(status(&namespace, &dsm).await, spec::Status::SUCCESS);

        dsm.cdw11 = nvm::Cdw11Dsm::new().with_ad(true).into();
        assert_eq!(
            status(&namespace, &dsm).await,
            spec::Status::ATTEMPTED_WRITE_TO_READ_ONLY_RANGE
        );
    }
}
