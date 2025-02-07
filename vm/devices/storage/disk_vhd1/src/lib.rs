// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A VHD1 disk implementation. Currently only supports fixed VHD1.

#![forbid(unsafe_code)]

use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedDisk;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend_resources::FixedVhd1DiskHandle;
use disk_file::FileDisk;
use guid::Guid;
use inspect::Inspect;
use scsi_buffers::RequestBuffers;
use std::fs::File;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use thiserror::Error;
use vhd1_defs::VhdFooter;
use vm_resource::declare_static_resolver;
use vm_resource::kind::DiskHandleKind;
use vm_resource::ResolveResource;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

pub struct Vhd1Resolver;
declare_static_resolver!(Vhd1Resolver, (DiskHandleKind, FixedVhd1DiskHandle));

#[derive(Debug, Error)]
pub enum ResolveVhd1DiskError {
    #[error("failed to open VHD")]
    Open(#[source] OpenError),
    #[error("invalid disk")]
    InvalidDisk(#[source] disk_backend::InvalidDisk),
}

impl ResolveResource<DiskHandleKind, FixedVhd1DiskHandle> for Vhd1Resolver {
    type Output = ResolvedDisk;
    type Error = ResolveVhd1DiskError;

    fn resolve(
        &self,
        rsrc: FixedVhd1DiskHandle,
        params: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let disk =
            Vhd1Disk::open_fixed(rsrc.0, params.read_only).map_err(ResolveVhd1DiskError::Open)?;
        ResolvedDisk::new(disk).map_err(ResolveVhd1DiskError::InvalidDisk)
    }
}

/// An open VHD1 disk.
#[derive(Debug, Inspect)]
pub struct Vhd1Disk {
    #[inspect(flatten)]
    file: FileDisk,
    unique_id: Guid,
}

const DEFAULT_SECTOR_SIZE: u32 = 512;
const DEFAULT_PHYSICAL_SECTOR_SIZE: u32 = 512;

#[derive(Debug)]
struct Metadata {
    disk_size: u64,
    sector_size: u32,
    unique_id: Guid,
}

impl Metadata {
    /// Parses the essential metadata out of the footer.
    fn from_footer(footer: VhdFooter, file_size: u64) -> Result<Metadata, OpenError> {
        if footer.cookie != VhdFooter::COOKIE_MAGIC {
            return Err(OpenError::InvalidFooterCookie);
        }
        if footer.checksum != footer.compute_checksum().to_be_bytes() {
            return Err(OpenError::InvalidFooterChecksum);
        }
        if footer.file_format_version != VhdFooter::FILE_FORMAT_VERSION_MAGIC.to_be_bytes() {
            return Err(OpenError::UnsupportedVersion(
                footer.file_format_version.into(),
            ));
        }
        // FUTURE: support parsing non-fixed VHDs.
        if footer.disk_type != VhdFooter::DISK_TYPE_FIXED.to_be_bytes() {
            return Err(OpenError::NotFixed);
        }
        let disk_size = footer.current_size.into();
        let sector_size = DEFAULT_SECTOR_SIZE;
        if disk_size > file_size - VhdFooter::LEN || disk_size % (sector_size as u64) != 0 {
            return Err(OpenError::InvalidDiskSize(disk_size));
        }

        let unique_id = footer.unique_id;
        Ok(Metadata {
            disk_size,
            sector_size,
            unique_id,
        })
    }
}

/// An error encountered while opening a VHD.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpenError {
    #[error("invalid VHD file size: {0}")]
    InvalidFileSize(u64),
    #[error("invalid VHD disk size: {0}")]
    InvalidDiskSize(u64),
    #[error("io error")]
    Io(#[from] io::Error),
    #[error("VHD file footer is missing")]
    InvalidFooterCookie,
    #[error("invalid VHD footer checksum")]
    InvalidFooterChecksum,
    #[error("unsupported VHD version: {0:#x}")]
    UnsupportedVersion(u32),
    #[error("not a fixed VHD")]
    NotFixed,
}

impl Vhd1Disk {
    /// Turns a raw image into a fixed VHD.
    pub fn make_fixed(mut file: &File) -> Result<(), OpenError> {
        let meta = file.metadata()?;
        let len = meta.len();
        if len % VhdFooter::ALIGNMENT != 0 {
            return Err(OpenError::InvalidDiskSize(len));
        }
        file.seek(io::SeekFrom::End(0))?;
        file.write_all(VhdFooter::new_fixed(len, Guid::new_random()).as_bytes())?;
        Ok(())
    }

    /// Opens a fixed VHD.
    pub fn open_fixed(mut file: File, read_only: bool) -> Result<Self, OpenError> {
        let meta = file.metadata()?;
        let len = meta.len();
        if len < VhdFooter::LEN || len % VhdFooter::ALIGNMENT != 0 {
            return Err(OpenError::InvalidFileSize(len));
        }
        file.seek(io::SeekFrom::End(-512))?;
        let mut footer: VhdFooter = FromZeros::new_zeroed();
        file.read_exact(footer.as_mut_bytes())?;
        let metadata = Metadata::from_footer(footer, len)?;

        // Just wrap FileDisk for handling actual IO.
        let file = FileDisk::with_metadata(
            file,
            disk_file::Metadata {
                disk_size: metadata.disk_size,
                sector_size: metadata.sector_size,
                physical_sector_size: DEFAULT_PHYSICAL_SECTOR_SIZE,
                read_only,
            },
        );

        Ok(Self {
            file,
            unique_id: metadata.unique_id,
        })
    }

    /// Drops the parsing state, returning the file handle.
    pub fn into_inner(self) -> File {
        self.file.into_inner()
    }
}

impl DiskIo for Vhd1Disk {
    fn disk_type(&self) -> &str {
        "vhd1"
    }

    fn sector_count(&self) -> u64 {
        self.file.sector_count()
    }

    fn sector_size(&self) -> u32 {
        self.file.sector_size()
    }

    fn is_read_only(&self) -> bool {
        self.file.is_read_only()
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        Some(self.unique_id.into())
    }

    fn physical_sector_size(&self) -> u32 {
        self.file.physical_sector_size()
    }

    fn is_fua_respected(&self) -> bool {
        self.file.is_fua_respected()
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        self.file.read_vectored(buffers, sector).await
    }

    async fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        self.file.write_vectored(buffers, sector, fua).await
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        self.file.sync_cache().await
    }

    async fn unmap(
        &self,
        _sector: u64,
        _count: u64,
        _block_level_only: bool,
    ) -> Result<(), DiskError> {
        Ok(())
    }

    fn unmap_behavior(&self) -> disk_backend::UnmapBehavior {
        disk_backend::UnmapBehavior::Ignored
    }
}

#[cfg(test)]
mod tests {
    use super::Vhd1Disk;
    use disk_backend::Disk;
    use guestmem::GuestMemory;
    use pal_async::async_test;
    use scsi_buffers::OwnedRequestBuffers;
    use std::io::Write;
    use zerocopy::IntoBytes;

    #[async_test]
    async fn open_fixed() {
        let mut file = tempfile::tempfile().unwrap();
        let data = (0..0x100000_u32).collect::<Vec<_>>();
        file.write_all(data.as_bytes()).unwrap();
        Vhd1Disk::make_fixed(&file).unwrap();
        let vhd = Disk::new(Vhd1Disk::open_fixed(file, false).unwrap()).unwrap();

        let mem = GuestMemory::allocate(0x1000);

        let mut buf = [0_u32; 128];
        vhd.read_vectored(
            &OwnedRequestBuffers::linear(0, 512, true).buffer(&mem),
            1000,
        )
        .await
        .unwrap();
        mem.read_at(0, buf.as_mut_bytes()).unwrap();
        assert!(buf.iter().copied().eq(1000_u32 * 128..1001 * 128));
    }
}
