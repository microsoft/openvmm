// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![cfg_attr(not(windows), forbid(unsafe_code))]
#![cfg_attr(windows, deny(unsafe_code))]

mod readwriteat;
#[cfg(windows)]
mod windows;

use self::readwriteat::ReadWriteAt;
use blocking::unblock;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedDisk;
use disk_backend_resources::FileDiskHandle;
#[cfg(windows)]
use disk_backend_resources::PhysicalDiskHandle;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use inspect::Inspect;
use scsi_buffers::RequestBuffers;
use std::fs;
use std::sync::Arc;
use thiserror::Error;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::DiskHandleKind;

pub struct FileDiskResolver;
declare_static_resolver!(FileDiskResolver, (DiskHandleKind, FileDiskHandle));

/// Resolver for Windows physical disks, such as `\\.\PhysicalDriveN`.
#[cfg(windows)]
pub struct PhysicalDiskResolver;
#[cfg(windows)]
declare_static_resolver!(PhysicalDiskResolver, (DiskHandleKind, PhysicalDiskHandle));

#[cfg(windows)]
impl ResolveResource<DiskHandleKind, PhysicalDiskHandle> for PhysicalDiskResolver {
    type Output = ResolvedDisk;
    type Error = ResolveFileDiskError;

    fn resolve(
        &self,
        rsrc: PhysicalDiskHandle,
        input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        ResolvedDisk::new(
            FileDisk::open_physical_disk(rsrc.0, input.read_only)
                .map_err(ResolveFileDiskError::Io)?,
        )
        .map_err(ResolveFileDiskError::InvalidDisk)
    }
}

#[derive(Debug, Error)]
pub enum ResolveFileDiskError {
    #[error("i/o error")]
    Io(#[source] std::io::Error),
    #[error("invalid disk")]
    InvalidDisk(#[source] disk_backend::InvalidDisk),
}

impl ResolveResource<DiskHandleKind, FileDiskHandle> for FileDiskResolver {
    type Output = ResolvedDisk;
    type Error = ResolveFileDiskError;

    fn resolve(
        &self,
        rsrc: FileDiskHandle,
        input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        ResolvedDisk::new(
            FileDisk::open(rsrc.0, input.read_only).map_err(ResolveFileDiskError::Io)?,
        )
        .map_err(ResolveFileDiskError::InvalidDisk)
    }
}

#[derive(Debug, Inspect)]
pub struct FileDisk {
    file: Arc<fs::File>,
    metadata: Metadata,
    sector_shift: u32,
}

#[derive(Debug, Inspect)]
pub struct Metadata {
    pub disk_size: u64,
    pub sector_size: u32,
    pub physical_sector_size: u32,
    pub read_only: bool,
}

#[cfg(windows)]
impl std::os::windows::io::AsHandle for FileDisk {
    fn as_handle(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        self.file.as_handle()
    }
}

impl FileDisk {
    /// Opens a disk backed by a regular file.
    pub fn open(file: fs::File, read_only: bool) -> Result<Self, std::io::Error> {
        let metadata = Metadata {
            disk_size: file.metadata()?.len(),
            sector_size: 512,
            physical_sector_size: 4096,
            read_only,
        };
        Ok(Self::with_metadata(file, metadata))
    }

    /// Opens a disk backed by a Windows physical disk, such as
    /// `\\.\PhysicalDriveN`, querying its size and sector sizes from the disk.
    ///
    /// The disk must be opened for synchronous I/O, with or without
    /// `FILE_FLAG_NO_BUFFERING`; overlapped handles are rejected.
    #[cfg(windows)]
    pub fn open_physical_disk(file: fs::File, read_only: bool) -> Result<Self, std::io::Error> {
        // The I/O path issues blocking reads and writes, which cannot complete
        // on an overlapped handle.
        pal::windows::fs::ensure_synchronous(&file)?;
        let geometry = windows::query_disk_geometry(&file)?;
        let metadata = Metadata {
            disk_size: geometry.disk_size,
            sector_size: geometry.sector_size,
            physical_sector_size: geometry.physical_sector_size,
            read_only,
        };
        Ok(Self::with_metadata(file, metadata))
    }

    /// Opens the disk using the specified metadata.
    ///
    /// This ensures that no metadata queries are made to the file, which may be
    /// appropriate if this is wrapped in another disk implementation that
    /// retrieves metadata in another way.
    pub fn with_metadata(file: fs::File, metadata: Metadata) -> Self {
        assert!(metadata.sector_size.is_power_of_two());
        assert!(metadata.sector_size >= 512);
        let sector_shift = metadata.sector_size.trailing_zeros();
        FileDisk {
            file: Arc::new(file),
            metadata,
            sector_shift,
        }
    }

    pub fn into_inner(self) -> fs::File {
        Arc::try_unwrap(self.file).expect("no outstanding IOs")
    }
}

impl FileDisk {
    /// Returns a zeroed buffer of `len` bytes whose start is aligned for
    /// unbuffered I/O, which requires sector-aligned buffer addresses (for
    /// example, Windows handles opened with `FILE_FLAG_NO_BUFFERING`).
    ///
    /// The returned vector is over-allocated; the aligned region begins at the
    /// returned offset.
    fn aligned_buffer(&self, len: usize) -> (Vec<u8>, usize) {
        let align = self.metadata.physical_sector_size.max(4096) as usize;
        let buffer = vec![0; len + align];
        let offset = buffer.as_ptr().align_offset(align);
        assert!(offset < align);
        (buffer, offset)
    }

    pub async fn read(&self, buffers: &RequestBuffers<'_>, sector: u64) -> Result<(), DiskError> {
        if ((sector << self.sector_shift) + buffers.len() as u64) > self.metadata.disk_size {
            return Err(DiskError::IllegalBlock);
        }
        let len = buffers.len();
        let (mut buffer, start) = self.aligned_buffer(len);
        let file = self.file.clone();
        let offset = sector << self.sector_shift;
        let buffer = unblock(move || -> Result<_, std::io::Error> {
            let count = file.read_at(&mut buffer[start..start + len], offset)?;
            if count != len {
                Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "short disk read",
                ))
            } else {
                Ok(buffer)
            }
        })
        .await
        .map_err(DiskError::Io)?;
        buffers.writer().write(&buffer[start..start + len])?;
        Ok(())
    }

    pub async fn write(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        _fua: bool,
    ) -> Result<(), DiskError> {
        if ((sector << self.sector_shift) + buffers.len() as u64) > self.metadata.disk_size {
            return Err(DiskError::IllegalBlock);
        }
        let len = buffers.len();
        let (mut buffer, start) = self.aligned_buffer(len);
        let file = self.file.clone();
        buffers.reader().read(&mut buffer[start..start + len])?;
        let offset = sector << self.sector_shift;
        unblock(move || -> Result<(), std::io::Error> {
            let count = file.write_at(&buffer[start..start + len], offset)?;
            if count != len {
                Err(std::io::Error::other("short disk write"))
            } else {
                Ok(())
            }
        })
        .await
        .map_err(DiskError::Io)?;
        Ok(())
    }

    pub async fn flush(&self) -> Result<(), DiskError> {
        let file = self.file.clone();
        unblock(move || file.sync_all())
            .await
            .map_err(DiskError::Io)?;
        Ok(())
    }
}

impl DiskIo for FileDisk {
    fn disk_type(&self) -> &str {
        "file"
    }

    fn sector_count(&self) -> u64 {
        self.metadata.disk_size >> self.sector_shift
    }

    fn sector_size(&self) -> u32 {
        self.metadata.sector_size
    }

    fn is_read_only(&self) -> bool {
        self.metadata.read_only
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        None
    }

    fn physical_sector_size(&self) -> u32 {
        self.metadata.physical_sector_size
    }

    fn is_fua_respected(&self) -> bool {
        false
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        self.read(buffers, sector).await
    }

    async fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        self.write(buffers, sector, fua).await
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        self.flush().await
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
    use super::FileDisk;
    use disk_backend::Disk;
    use disk_backend::DiskError;
    use guestmem::GuestMemory;
    use pal_async::async_test;
    use scsi_buffers::OwnedRequestBuffers;

    const SECTOR_SIZE: usize = 512;
    const DISK_SIZE: u64 = 1024 * 1024;

    fn file_disk() -> Disk {
        let file = tempfile::tempfile().unwrap();
        file.set_len(DISK_SIZE).unwrap();
        Disk::new(FileDisk::open(file, false).unwrap()).unwrap()
    }

    #[async_test]
    async fn short_read_returns_error() {
        let file = tempfile::tempfile().unwrap();
        file.set_len(DISK_SIZE).unwrap();
        let disk = FileDisk::open(file.try_clone().unwrap(), false).unwrap();
        file.set_len((SECTOR_SIZE + SECTOR_SIZE / 2) as u64)
            .unwrap();

        let mem = GuestMemory::allocate(SECTOR_SIZE);
        mem.write_at(0, &[0xcd; SECTOR_SIZE]).unwrap();
        let buffers = OwnedRequestBuffers::linear(0, SECTOR_SIZE, true);
        let result = disk.read(&buffers.buffer(&mem), 1).await;

        assert!(
            matches!(result, Err(DiskError::Io(ref error)) if error.kind() == std::io::ErrorKind::UnexpectedEof),
            "{result:?}"
        );
        let mut contents = [0; SECTOR_SIZE];
        mem.read_at(0, &mut contents).unwrap();
        assert_eq!(contents, [0xcd; SECTOR_SIZE]);
    }

    #[async_test]
    async fn sector_range_conformance() {
        storage_tests::sector_range::test_disk_sector_range_conformance(&file_disk()).await;
    }

    /// Unbuffered handles require sector-aligned buffer addresses, which
    /// ordinary heap buffers do not guarantee.
    #[test]
    fn io_buffers_are_sector_aligned() {
        let file = tempfile::tempfile().unwrap();
        file.set_len(DISK_SIZE).unwrap();
        let disk = FileDisk::with_metadata(
            file,
            super::Metadata {
                disk_size: DISK_SIZE,
                sector_size: 4096,
                physical_sector_size: 8192,
                read_only: false,
            },
        );
        for len in [4096, 8192, 65536] {
            let (buffer, start) = disk.aligned_buffer(len);
            let region = &buffer[start..start + len];
            assert_eq!(region.as_ptr() as usize % 8192, 0, "len {len}");
        }
    }

    /// Physical-disk opens query the disk directly, so a regular file is
    /// rejected rather than treated as a file-backed disk.
    #[cfg(windows)]
    #[test]
    fn physical_disk_open_rejects_regular_file() {
        let file = tempfile::tempfile().unwrap();
        file.set_len(DISK_SIZE).unwrap();
        FileDisk::open_physical_disk(file, false).unwrap_err();
    }

    /// Physical-disk I/O blocks, so overlapped handles are rejected before any
    /// device query.
    #[cfg(windows)]
    #[test]
    fn physical_disk_open_rejects_overlapped_handle() {
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
        let err = FileDisk::open_physical_disk(file, false).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("synchronous I/O"), "{err}");
    }

    /// Unbuffered handles round-trip guest I/O.
    #[cfg(windows)]
    #[async_test]
    async fn unbuffered_handle_round_trips() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_NO_BUFFERING;

        // Unbuffered transfers must also be multiples of the host volume's
        // sector size, which may be 4 KiB.
        const IO_SIZE: usize = 4096;
        const SECTOR: u64 = (IO_SIZE / SECTOR_SIZE) as u64;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.img");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(DISK_SIZE)
            .unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_NO_BUFFERING)
            .open(&path)
            .unwrap();
        let disk = Disk::new(FileDisk::open(file, false).unwrap()).unwrap();

        let mem = GuestMemory::allocate(IO_SIZE * 2);
        mem.write_at(0, &[0xa5; IO_SIZE]).unwrap();
        disk.write_vectored(
            &OwnedRequestBuffers::linear(0, IO_SIZE, false).buffer(&mem),
            SECTOR,
            false,
        )
        .await
        .unwrap();
        disk.read_vectored(
            &OwnedRequestBuffers::linear(IO_SIZE as u64, IO_SIZE, true).buffer(&mem),
            SECTOR,
        )
        .await
        .unwrap();

        let mut contents = [0; IO_SIZE];
        mem.read_at(IO_SIZE as u64, &mut contents).unwrap();
        assert_eq!(contents, [0xa5; IO_SIZE]);
    }

    /// The range check here is in byte units, computing `sector << sector_shift`.
    /// A left shift discards high bits without panicking, so a large enough
    /// sector wraps to a small byte offset and passes the check.
    ///
    /// This is the one defect in this area whose symptom was silent wrong data
    /// rather than a panic, so the test asserts both that the request is
    /// rejected and that it did not return the contents of sector 0.
    #[async_test]
    async fn sector_does_not_wrap_when_shifted() {
        let disk = file_disk();
        let mem = GuestMemory::allocate(SECTOR_SIZE);

        mem.write_at(0, &[0xcd; SECTOR_SIZE]).unwrap();
        disk.write_vectored(
            &OwnedRequestBuffers::linear(0, SECTOR_SIZE, false).buffer(&mem),
            0,
            false,
        )
        .await
        .unwrap();
        mem.write_at(0, &[0; SECTOR_SIZE]).unwrap();

        // `1 << 55` shifted left by 9 (512-byte sectors) is `1 << 64`, which
        // truncates to a byte offset of zero.
        let r = disk
            .read_vectored(
                &OwnedRequestBuffers::linear(0, SECTOR_SIZE, true).buffer(&mem),
                1 << 55,
            )
            .await;

        let mut buf = [0; SECTOR_SIZE];
        mem.read_at(0, &mut buf).unwrap();
        assert_ne!(buf, [0xcd; SECTOR_SIZE], "read returned sector 0");
        assert!(matches!(r, Err(DiskError::IllegalBlock)), "{r:?}");
    }
}
