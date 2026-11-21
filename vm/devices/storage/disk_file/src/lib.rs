// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

mod readwriteat;

use self::readwriteat::ReadWriteAt;
use blocking::unblock;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedDisk;
use disk_backend_resources::FileDiskHandle;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use inspect::Inspect;
use scsi_buffers::BounceBuffer;
use scsi_buffers::RequestBuffers;
use std::fs;
use std::sync::Arc;
use thiserror::Error;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::DiskHandleKind;

pub struct FileDiskResolver;
declare_static_resolver!(FileDiskResolver, (DiskHandleKind, FileDiskHandle));

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
    aligned_buffers: bool,
}

#[derive(Debug, Inspect)]
pub struct Metadata {
    pub disk_size: u64,
    pub sector_size: u32,
    pub physical_sector_size: u32,
    pub read_only: bool,
}

/// A zeroed I/O buffer.
enum FileIoBuffer {
    /// A plain heap allocation.
    Unaligned(Vec<u8>),
    /// A buffer aligned to [`BounceBuffer::ALIGNMENT`], for handles opened for
    /// unbuffered I/O.
    Aligned(BounceBuffer),
}

impl FileIoBuffer {
    /// Returns the buffer memory.
    ///
    /// This requires exclusive access because [`BounceBuffer`] also exposes
    /// its memory as shared atomics.
    fn as_mut_bytes(&mut self) -> &mut [u8] {
        match self {
            FileIoBuffer::Unaligned(buffer) => buffer,
            FileIoBuffer::Aligned(buffer) => buffer.as_mut_bytes(),
        }
    }
}

#[cfg(windows)]
impl std::os::windows::io::AsHandle for FileDisk {
    fn as_handle(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        self.file.as_handle()
    }
}

impl FileDisk {
    pub fn open(file: fs::File, read_only: bool) -> Result<Self, std::io::Error> {
        let metadata = Metadata {
            disk_size: file.metadata()?.len(),
            sector_size: 512,
            physical_sector_size: 4096,
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
            aligned_buffers: false,
        }
    }

    /// Allocates I/O buffers aligned to [`BounceBuffer::ALIGNMENT`], as
    /// required for handles opened with `FILE_FLAG_NO_BUFFERING` or
    /// `O_DIRECT`.
    ///
    /// `required_alignment` is the buffer alignment, in bytes, that the
    /// underlying device enforces. Fails if it or the sector size exceeds
    /// [`BounceBuffer::ALIGNMENT`], so that the disk fails to open rather than
    /// failing guest I/O.
    pub fn with_aligned_buffers(mut self, required_alignment: u32) -> Result<Self, std::io::Error> {
        for (what, size) in [
            ("sector size", self.metadata.sector_size),
            ("device buffer alignment", required_alignment),
        ] {
            if size as usize > BounceBuffer::ALIGNMENT {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    format!(
                        "{what} {size} exceeds the I/O buffer alignment {}",
                        BounceBuffer::ALIGNMENT
                    ),
                ));
            }
        }
        self.aligned_buffers = true;
        Ok(self)
    }

    pub fn into_inner(self) -> fs::File {
        Arc::try_unwrap(self.file).expect("no outstanding IOs")
    }
}

impl FileDisk {
    fn io_buffer(&self, len: usize) -> FileIoBuffer {
        if self.aligned_buffers {
            FileIoBuffer::Aligned(BounceBuffer::new(len))
        } else {
            FileIoBuffer::Unaligned(vec![0; len])
        }
    }

    pub async fn read(&self, buffers: &RequestBuffers<'_>, sector: u64) -> Result<(), DiskError> {
        if ((sector << self.sector_shift) + buffers.len() as u64) > self.metadata.disk_size {
            return Err(DiskError::IllegalBlock);
        }
        let mut buffer = self.io_buffer(buffers.len());
        let file = self.file.clone();
        let offset = sector << self.sector_shift;
        let mut buffer = unblock(move || -> Result<_, std::io::Error> {
            let bytes = buffer.as_mut_bytes();
            let count = file.read_at(bytes, offset)?;
            if count != bytes.len() {
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
        buffers.writer().write(buffer.as_mut_bytes())?;
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
        let mut buffer = self.io_buffer(buffers.len());
        let file = self.file.clone();
        buffers.reader().read(buffer.as_mut_bytes())?;
        let offset = sector << self.sector_shift;
        unblock(move || -> Result<(), std::io::Error> {
            let bytes = buffer.as_mut_bytes();
            let count = file.write_at(bytes, offset)?;
            if count != bytes.len() {
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
    use super::Metadata;
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
    /// Aligned buffers cannot satisfy sector sizes or device alignment
    /// requirements larger than their own alignment.
    #[test]
    fn aligned_buffers_reject_oversized_sectors() {
        let alignment = scsi_buffers::BounceBuffer::ALIGNMENT as u32;
        let disk = |sector_size| {
            FileDisk::with_metadata(
                tempfile::tempfile().unwrap(),
                Metadata {
                    disk_size: DISK_SIZE,
                    sector_size,
                    physical_sector_size: sector_size,
                    read_only: false,
                },
            )
        };
        for required_alignment in [1, 512, alignment] {
            disk(512).with_aligned_buffers(required_alignment).unwrap();
        }
        for (sector_size, required_alignment) in [(alignment * 2, 512), (512, alignment * 2)] {
            let err = disk(sector_size)
                .with_aligned_buffers(required_alignment)
                .unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
        }
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
