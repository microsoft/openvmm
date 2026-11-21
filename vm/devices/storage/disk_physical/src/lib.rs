// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A disk backed by a Windows physical disk, such as `\\.\PhysicalDriveN`.
//!
//! The disk's size and sector sizes are queried from the device. I/O is
//! performed by `disk_file::FileDisk`, so the handle must be opened for
//! synchronous I/O, with or without `FILE_FLAG_NO_BUFFERING`.

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
    /// Fails if `file` is not a disk device, was opened for overlapped I/O, or
    /// requires buffers more strictly aligned than [`BounceBuffer::ALIGNMENT`].
    pub fn open(file: File, read_only: bool) -> std::io::Result<Self> {
        // FileDisk issues blocking reads and writes, which cannot complete on
        // an overlapped handle.
        pal::windows::fs::ensure_synchronous(&file)?;
        let geometry = geometry::query_disk_geometry(&file)?;
        check_buffer_alignment(geometry::query_buffer_alignment(&file)?)?;
        // The handle may have been opened for unbuffered I/O, which requires
        // sector-aligned buffers.
        let file = FileDisk::with_metadata(
            file,
            disk_file::Metadata {
                disk_size: geometry.disk_size,
                sector_size: geometry.sector_size,
                physical_sector_size: geometry.physical_sector_size,
                read_only,
            },
        )
        .with_aligned_buffers()?;
        Ok(Self { file })
    }
}

/// Fails if the device requires buffers more strictly aligned than the I/O
/// buffers provide, so that the disk fails to open rather than failing guest
/// I/O.
fn check_buffer_alignment(alignment: u32) -> std::io::Result<()> {
    if alignment as usize > BounceBuffer::ALIGNMENT {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!(
                "device requires {alignment}-byte buffer alignment, more than the supported {}",
                BounceBuffer::ALIGNMENT
            ),
        ));
    }
    Ok(())
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
    use pal_async::async_test;

    const DISK_SIZE: u64 = 1024 * 1024;

    /// Geometry is queried from the device, so a regular file is rejected
    /// rather than treated as a file-backed disk.
    #[test]
    fn rejects_regular_file() {
        let file = tempfile::tempfile().unwrap();
        file.set_len(DISK_SIZE).unwrap();
        PhysicalDisk::open(file, false).unwrap_err();
    }

    /// Devices requiring more buffer alignment than the I/O buffers provide
    /// are rejected, regardless of their sector sizes.
    #[test]
    fn buffer_alignment_is_checked() {
        let alignment = scsi_buffers::BounceBuffer::ALIGNMENT as u32;
        for supported in [1, 512, alignment] {
            super::check_buffer_alignment(supported).unwrap();
        }
        let err = super::check_buffer_alignment(alignment * 2).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
    }

    /// The device's alignment requirement is reported as a power of two.
    #[test]
    fn buffer_alignment_is_queried() {
        let file = tempfile::tempfile().unwrap();
        let alignment = super::geometry::query_buffer_alignment(&file).unwrap();
        assert!(alignment.is_power_of_two(), "{alignment}");
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

    /// Geometry is queried from a real disk, and unbuffered I/O round-trips.
    ///
    /// Attaching a VHD as a host disk requires administrator privileges. Run
    /// with `cargo nextest run -p disk_physical --run-ignored all`.
    #[async_test]
    #[ignore = "needs administrator to attach a VHD as a host disk"]
    async fn attached_vhd_round_trips() {
        use disk_backend::DiskIo;
        use guestmem::GuestMemory;
        use scsi_buffers::OwnedRequestBuffers;
        use std::os::windows::fs::FileExt;
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_NO_BUFFERING;

        const IO_SIZE: usize = 4096;
        const SECTOR: u64 = 8;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.vhd");
        {
            let image = std::fs::File::create(&path).unwrap();
            image.set_len(DISK_SIZE).unwrap();
            image.seek_write(&[0x5a; IO_SIZE], SECTOR * 512).unwrap();
            disk_vhd1::Vhd1Disk::make_fixed(&image).unwrap();
        }
        let (_vhd, disk_path) = disk_vhdmp::Vhd::attach_to_host(&path, false).unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_NO_BUFFERING)
            .open(&disk_path)
            .unwrap();
        let disk = PhysicalDisk::open(file, false).unwrap();
        assert_eq!(disk.sector_size(), 512);
        assert!(disk.physical_sector_size() >= 512);
        assert_eq!(disk.sector_count() * 512, DISK_SIZE);

        let mem = GuestMemory::allocate(IO_SIZE * 2);
        disk.read_vectored(
            &OwnedRequestBuffers::linear(0, IO_SIZE, true).buffer(&mem),
            SECTOR,
        )
        .await
        .unwrap();
        let mut contents = [0; IO_SIZE];
        mem.read_at(0, &mut contents).unwrap();
        assert_eq!(contents, [0x5a; IO_SIZE]);

        mem.write_at(IO_SIZE as u64, &[0xa5; IO_SIZE]).unwrap();
        disk.write_vectored(
            &OwnedRequestBuffers::linear(IO_SIZE as u64, IO_SIZE, false).buffer(&mem),
            SECTOR * 2,
            false,
        )
        .await
        .unwrap();
        disk.read_vectored(
            &OwnedRequestBuffers::linear(0, IO_SIZE, true).buffer(&mem),
            SECTOR * 2,
        )
        .await
        .unwrap();
        mem.read_at(0, &mut contents).unwrap();
        assert_eq!(contents, [0xa5; IO_SIZE]);
    }
}
