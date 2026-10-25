// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A disk of fixed geometry that reads zeros past the end of its backing disk
//! and refuses writes.
//!
//! [`ZeroFillDisk`] presents a fixed sector count over an optional backing
//! [`Disk`] with the same sector sizes and no more sectors. Reads of the
//! sectors that the backing disk has go to it. Reads of the sectors past its
//! end, or of every sector of a disk without one, return zeros, including the
//! tail of a request that crosses the end. Writes fail without reaching the
//! backing disk, so the disk never resizes, pads, or writes it, and keeps no
//! written data.
//!
//! A microVM platform restore uses it to present the captured capacity of its
//! `distro` block over a shorter image, and to attach a fresh scratch block
//! that has no file.

#![forbid(unsafe_code)]

pub mod resolver;

use disk_backend::Disk;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::UnmapBehavior;
use guestmem::MemoryWrite;
use inspect::Inspect;
use scsi_buffers::RequestBuffers;
use thiserror::Error;

/// A disk of fixed geometry that reads zeros past the end of its backing disk
/// and refuses writes.
#[derive(Inspect)]
pub struct ZeroFillDisk {
    backing: Option<Disk>,
    backing_sector_count: u64,
    sector_count: u64,
    sector_size: u32,
    #[inspect(skip)]
    sector_shift: u32,
    physical_sector_size: u32,
    read_only: bool,
}

/// An error returned by [`ZeroFillDisk::new`].
#[derive(Debug, Error)]
pub enum NewZeroFillDiskError {
    /// The sector size is not a power of two of at least 512 bytes.
    #[error("invalid sector size {0}")]
    InvalidSectorSize(u32),
    /// The physical sector size is not a power of two of at least the sector
    /// size.
    #[error("invalid physical sector size {0}")]
    InvalidPhysicalSectorSize(u32),
    /// The backing disk's sector sizes differ from the disk's.
    #[error(
        "backing disk sector sizes {backing_sector_size}/{backing_physical_sector_size} do not match {sector_size}/{physical_sector_size}"
    )]
    SectorSizeMismatch {
        /// The disk's sector size.
        sector_size: u32,
        /// The disk's physical sector size.
        physical_sector_size: u32,
        /// The backing disk's sector size.
        backing_sector_size: u32,
        /// The backing disk's physical sector size.
        backing_physical_sector_size: u32,
    },
    /// The backing disk has more sectors than the disk.
    #[error("backing disk has {backing_sector_count} sectors, more than the disk's {sector_count}")]
    BackingTooLong {
        /// The disk's sector count.
        sector_count: u64,
        /// The backing disk's sector count.
        backing_sector_count: u64,
    },
}

impl ZeroFillDisk {
    /// Returns a disk of `sector_count` sectors of `sector_size` bytes, with
    /// physical sectors of `physical_sector_size` bytes, that reads the
    /// sectors that `backing` has from it and zeros past them.
    ///
    /// `backing` must have the same sector sizes and at most `sector_count`
    /// sectors. Its sector count is sampled here: a backing disk that resizes
    /// later does not change which sectors read as zeros, and this disk never
    /// resizes. `read_only` selects how writes fail: with
    /// [`DiskError::ReadOnly`] for a read-only disk, and with
    /// [`DiskError::Io`] for a writable one, which the guest negotiated as
    /// writable but which has nowhere to keep its data.
    pub fn new(
        backing: Option<Disk>,
        sector_count: u64,
        sector_size: u32,
        physical_sector_size: u32,
        read_only: bool,
    ) -> Result<Self, NewZeroFillDiskError> {
        if sector_size < 512 || !sector_size.is_power_of_two() {
            return Err(NewZeroFillDiskError::InvalidSectorSize(sector_size));
        }
        if physical_sector_size < sector_size || !physical_sector_size.is_power_of_two() {
            return Err(NewZeroFillDiskError::InvalidPhysicalSectorSize(
                physical_sector_size,
            ));
        }
        let backing_sector_count = if let Some(backing) = &backing {
            if backing.sector_size() != sector_size
                || backing.physical_sector_size() != physical_sector_size
            {
                return Err(NewZeroFillDiskError::SectorSizeMismatch {
                    sector_size,
                    physical_sector_size,
                    backing_sector_size: backing.sector_size(),
                    backing_physical_sector_size: backing.physical_sector_size(),
                });
            }
            let backing_sector_count = backing.sector_count();
            if backing_sector_count > sector_count {
                return Err(NewZeroFillDiskError::BackingTooLong {
                    sector_count,
                    backing_sector_count,
                });
            }
            backing_sector_count
        } else {
            0
        };
        Ok(Self {
            backing,
            backing_sector_count,
            sector_count,
            sector_size,
            sector_shift: sector_size.trailing_zeros(),
            physical_sector_size,
            read_only,
        })
    }

    /// Fails a request of `count` sectors at `sector` that does not lie inside
    /// the disk.
    fn check_range(&self, sector: u64, count: u64) -> Result<(), DiskError> {
        if sector
            .checked_add(count)
            .is_none_or(|end| end > self.sector_count)
        {
            return Err(DiskError::IllegalBlock);
        }
        Ok(())
    }

    /// Returns the error of a write that lies inside the disk.
    fn refused_write(&self) -> DiskError {
        if self.read_only {
            DiskError::ReadOnly
        } else {
            DiskError::Io(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "zero-fill disk refuses writes",
            ))
        }
    }
}

impl DiskIo for ZeroFillDisk {
    fn disk_type(&self) -> &str {
        "zerofill"
    }

    fn sector_count(&self) -> u64 {
        self.sector_count
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        self.backing.as_ref().and_then(Disk::disk_id)
    }

    fn physical_sector_size(&self) -> u32 {
        self.physical_sector_size
    }

    fn is_fua_respected(&self) -> bool {
        false
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        let count = (buffers.len() >> self.sector_shift) as u64;
        self.check_range(sector, count)?;
        // The backing disk serves the sectors that it has, and the rest of the
        // request reads as zeros.
        let backed = self.backing_sector_count.saturating_sub(sector).min(count);
        let backed_len = (backed << self.sector_shift) as usize;
        if let Some(backing) = &self.backing
            && backed != 0
        {
            backing
                .read_vectored(&buffers.subrange(0, backed_len), sector)
                .await?;
        }
        let zero_len = buffers.len() - backed_len;
        if zero_len != 0 {
            buffers
                .subrange(backed_len, zero_len)
                .writer()
                .zero(zero_len)?;
        }
        Ok(())
    }

    async fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        _fua: bool,
    ) -> Result<(), DiskError> {
        self.check_range(sector, (buffers.len() >> self.sector_shift) as u64)?;
        Err(self.refused_write())
    }

    /// Succeeds: no write ever succeeds, so no data waits to be flushed.
    async fn sync_cache(&self) -> Result<(), DiskError> {
        Ok(())
    }

    /// Changes nothing: the disk ignores unmaps, as its unmap behavior
    /// reports.
    async fn unmap(
        &self,
        sector: u64,
        count: u64,
        _block_level_only: bool,
    ) -> Result<(), DiskError> {
        self.check_range(sector, count)
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        UnmapBehavior::Ignored
    }
}

#[cfg(test)]
mod tests {
    use super::NewZeroFillDiskError;
    use super::ZeroFillDisk;
    use disk_backend::Disk;
    use disk_backend::DiskError;
    use disk_backend::DiskIo;
    use guestmem::GuestMemory;
    use pal_async::async_test;
    use scsi_buffers::OwnedRequestBuffers;

    const SECTOR_SIZE: usize = 512;

    /// Returns the contents of a `count`-sector disk whose sector `n` is full
    /// of the byte `n + 1`.
    fn pattern(count: usize) -> Vec<u8> {
        (0..count)
            .flat_map(|sector| [sector as u8 + 1; SECTOR_SIZE])
            .collect()
    }

    async fn write(disk: &Disk, sector: u64, data: &[u8]) -> Result<(), DiskError> {
        let mem = GuestMemory::allocate(data.len());
        mem.write_at(0, data).unwrap();
        disk.write_vectored(
            &OwnedRequestBuffers::linear(0, data.len(), false).buffer(&mem),
            sector,
            false,
        )
        .await
    }

    /// Reads `count` sectors at `sector` into a buffer that was full of ones.
    async fn read(disk: &Disk, sector: u64, count: usize) -> Vec<u8> {
        let mem = GuestMemory::allocate(count * SECTOR_SIZE);
        mem.fill_at(0, 0xff, count * SECTOR_SIZE).unwrap();
        disk.read_vectored(
            &OwnedRequestBuffers::linear(0, count * SECTOR_SIZE, true).buffer(&mem),
            sector,
        )
        .await
        .unwrap();
        let mut data = vec![0; count * SECTOR_SIZE];
        mem.read_at(0, &mut data).unwrap();
        data
    }

    /// Returns a writable RAM disk of `count` sectors with the contents of
    /// [`pattern`].
    async fn backing(count: usize) -> Disk {
        let disk = disklayer_ram::ram_disk((count * SECTOR_SIZE) as u64, false).unwrap();
        write(&disk, 0, &pattern(count)).await.unwrap();
        disk
    }

    fn zero_fill(backing: Option<Disk>, sector_count: u64, read_only: bool) -> Disk {
        Disk::new(
            ZeroFillDisk::new(
                backing,
                sector_count,
                SECTOR_SIZE as u32,
                SECTOR_SIZE as u32,
                read_only,
            )
            .unwrap(),
        )
        .unwrap()
    }

    #[async_test]
    async fn reads_the_backing_disk_then_zeros() {
        let disk = zero_fill(Some(backing(4).await), 8, true);
        let image = pattern(4);
        let zeros = |count| vec![0; count * SECTOR_SIZE];
        assert_eq!(disk.sector_count(), 8);

        // Inside the backing disk.
        assert_eq!(read(&disk, 0, 4).await, image);
        assert_eq!(read(&disk, 3, 1).await, image[3 * SECTOR_SIZE..]);
        // Across its end.
        assert_eq!(
            read(&disk, 2, 4).await,
            [&image[2 * SECTOR_SIZE..], &zeros(2)].concat()
        );
        assert_eq!(read(&disk, 0, 8).await, [image, zeros(4)].concat());
        // Past it, up to the disk's last sector.
        assert_eq!(read(&disk, 4, 4).await, zeros(4));
        assert_eq!(read(&disk, 7, 1).await, zeros(1));
    }

    #[async_test]
    async fn a_disk_without_backing_reads_zeros() {
        let disk = zero_fill(None, 8, false);
        assert_eq!(disk.sector_count(), 8);
        assert_eq!(disk.sector_size(), SECTOR_SIZE as u32);
        assert!(!disk.is_read_only());
        assert_eq!(disk.disk_id(), None);
        assert_eq!(read(&disk, 0, 8).await, vec![0; 8 * SECTOR_SIZE]);
    }

    #[async_test]
    async fn writes_are_refused_without_reaching_the_backing_disk() {
        let image = pattern(4);

        // A read-only disk refuses writes inside and across the backing disk.
        let inner = backing(4).await;
        let disk = zero_fill(Some(inner.clone()), 8, true);
        for sector in [0, 3, 6] {
            assert!(matches!(
                write(&disk, sector, &[0x55; 2 * SECTOR_SIZE]).await,
                Err(DiskError::ReadOnly)
            ));
        }
        assert_eq!(read(&inner, 0, 4).await, image);
        assert_eq!(
            read(&disk, 0, 8).await,
            [&image[..], &[0; 4 * SECTOR_SIZE]].concat()
        );

        // A writable disk fails them as I/O errors, and keeps nothing.
        let inner = backing(4).await;
        let disk = zero_fill(Some(inner.clone()), 8, false);
        for sector in [0, 3, 6] {
            assert!(matches!(
                write(&disk, sector, &[0x55; 2 * SECTOR_SIZE]).await,
                Err(DiskError::Io(_))
            ));
        }
        assert_eq!(read(&inner, 0, 4).await, image);
        assert_eq!(
            read(&disk, 0, 8).await,
            [&image[..], &[0; 4 * SECTOR_SIZE]].concat()
        );

        let disk = zero_fill(None, 8, false);
        assert!(matches!(
            write(&disk, 0, &[0x55; SECTOR_SIZE]).await,
            Err(DiskError::Io(_))
        ));
        assert_eq!(read(&disk, 0, 1).await, vec![0; SECTOR_SIZE]);
        // Nothing waits to be flushed, and unmaps change nothing.
        disk.sync_cache().await.unwrap();
        disk.unmap(0, 8, false).await.unwrap();
        assert_eq!(read(&disk, 0, 8).await, vec![0; 8 * SECTOR_SIZE]);
        // The backend ignores the unmaps that it could receive, as `Disk` does.
        let backend = ZeroFillDisk::new(Some(inner.clone()), 8, 512, 512, true).unwrap();
        DiskIo::unmap(&backend, 0, 8, false).await.unwrap();
        assert!(matches!(
            DiskIo::unmap(&backend, 4, 5, false).await,
            Err(DiskError::IllegalBlock)
        ));
        assert_eq!(read(&inner, 0, 4).await, image);
    }

    #[async_test]
    async fn sector_range_conformance() {
        let disk = zero_fill(Some(backing(4).await), 8, true);
        storage_tests::sector_range::test_disk_sector_range_conformance(&disk).await;
        let disk = zero_fill(None, 8, false);
        storage_tests::sector_range::test_disk_sector_range_conformance(&disk).await;
    }

    #[async_test]
    async fn new_rejects_inconsistent_geometry() {
        // A backing disk as long as the disk is accepted, and a longer one is
        // not.
        ZeroFillDisk::new(Some(backing(8).await), 8, 512, 512, true).unwrap();
        assert!(matches!(
            ZeroFillDisk::new(Some(backing(8).await), 7, 512, 512, true),
            Err(NewZeroFillDiskError::BackingTooLong {
                sector_count: 7,
                backing_sector_count: 8,
            })
        ));
        // The sector sizes must match the backing disk's.
        assert!(matches!(
            ZeroFillDisk::new(Some(backing(8).await), 8, 512, 4096, true),
            Err(NewZeroFillDiskError::SectorSizeMismatch { .. })
        ));
        assert!(matches!(
            ZeroFillDisk::new(Some(backing(8).await), 8, 4096, 4096, true),
            Err(NewZeroFillDiskError::SectorSizeMismatch { .. })
        ));
        // And they must be valid.
        for sector_size in [0, 256, 768] {
            assert!(matches!(
                ZeroFillDisk::new(None, 8, sector_size, 4096, false),
                Err(NewZeroFillDiskError::InvalidSectorSize(_))
            ));
        }
        for physical_sector_size in [256, 1536] {
            assert!(matches!(
                ZeroFillDisk::new(None, 8, 512, physical_sector_size, false),
                Err(NewZeroFillDiskError::InvalidPhysicalSectorSize(_))
            ));
        }
        let disk = ZeroFillDisk::new(None, 8, 512, 4096, false).unwrap();
        let disk = Disk::new(disk).unwrap();
        assert_eq!(disk.physical_sector_size(), 4096);
    }
}
