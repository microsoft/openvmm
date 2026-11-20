// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A disk backed by a Windows physical disk, such as `\\.\PhysicalDriveN`.
//!
//! The disk's size and sector sizes are queried from the device. I/O is
//! performed by [`FileDisk`], so the handle must be opened for synchronous
//! I/O, with or without `FILE_FLAG_NO_BUFFERING`.

#![cfg(windows)]
#![deny(unsafe_code)]

mod geometry;

use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedDisk;
use disk_backend_resources::PhysicalDiskHandle;
use disk_file::FileDisk;
use inspect::Inspect;
use scsi_buffers::BounceBuffer;
use scsi_buffers::RequestBuffers;
use std::fs::File;
use thiserror::Error;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::DiskHandleKind;

/// Resolver for [`PhysicalDiskHandle`].
pub struct PhysicalDiskResolver;
declare_static_resolver!(PhysicalDiskResolver, (DiskHandleKind, PhysicalDiskHandle));

/// An error resolving a [`PhysicalDiskHandle`].
#[derive(Debug, Error)]
pub enum ResolvePhysicalDiskError {
    /// The physical disk could not be opened.
    #[error("failed to open physical disk")]
    Open(#[source] std::io::Error),
    /// The disk is invalid.
    #[error("invalid disk")]
    InvalidDisk(#[source] disk_backend::InvalidDisk),
}

impl ResolveResource<DiskHandleKind, PhysicalDiskHandle> for PhysicalDiskResolver {
    type Output = ResolvedDisk;
    type Error = ResolvePhysicalDiskError;

    fn resolve(
        &self,
        rsrc: PhysicalDiskHandle,
        input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        ResolvedDisk::new(
            PhysicalDisk::open(rsrc.0, input.read_only).map_err(ResolvePhysicalDiskError::Open)?,
        )
        .map_err(ResolvePhysicalDiskError::InvalidDisk)
    }
}

/// A disk backed by a Windows physical disk.
#[derive(Debug, Inspect)]
pub struct PhysicalDisk {
    #[inspect(flatten)]
    file: FileDisk,
}

impl PhysicalDisk {
    /// Opens a physical disk, querying its size and sector sizes.
    ///
    /// Fails if `file` is not a disk device, or was opened for overlapped I/O.
    pub fn open(file: File, read_only: bool) -> std::io::Result<Self> {
        // FileDisk issues blocking reads and writes, which cannot complete on
        // an overlapped handle.
        pal::windows::fs::ensure_synchronous(&file)?;
        let geometry = geometry::query_disk_geometry(&file)?;
        // Unbuffered I/O requires sector-aligned buffers, which aligned
        // buffers provide only up to their fixed alignment.
        if geometry.sector_size as usize > BounceBuffer::ALIGNMENT {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!("unsupported sector size {}", geometry.sector_size),
            ));
        }
        let file = FileDisk::with_metadata(
            file,
            disk_file::Metadata {
                disk_size: geometry.disk_size,
                sector_size: geometry.sector_size,
                physical_sector_size: geometry.physical_sector_size,
                read_only,
            },
        )
        .with_aligned_buffers();
        Ok(Self { file })
    }
}

impl DiskIo for PhysicalDisk {
    fn disk_type(&self) -> &str {
        "physical"
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
        None
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
    use super::PhysicalDisk;

    const DISK_SIZE: u64 = 1024 * 1024;

    /// Geometry is queried from the device, so a regular file is rejected
    /// rather than treated as a file-backed disk.
    #[test]
    fn rejects_regular_file() {
        let file = tempfile::tempfile().unwrap();
        file.set_len(DISK_SIZE).unwrap();
        PhysicalDisk::open(file, false).unwrap_err();
    }

    /// I/O blocks, so overlapped handles are rejected before any device query.
    #[test]
    fn rejects_overlapped_handle() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.img");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(DISK_SIZE)
            .unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OVERLAPPED)
            .open(&path)
            .unwrap();
        let err = PhysicalDisk::open(file, false).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("synchronous I/O"), "{err}");
    }
}
