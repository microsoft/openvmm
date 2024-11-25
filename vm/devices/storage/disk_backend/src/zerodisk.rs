// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A read-only disk that always returns zero on read.

use super::DiskIo;
use crate::DiskError;
use guestmem::MemoryWrite;
use inspect::Inspect;
use scsi_buffers::RequestBuffers;

/// A read-only disk that always returns zero on read.
#[derive(Debug, Inspect)]
pub struct ZeroDisk {
    sector_size: u32,
    sector_count: u64,
}

/// Error returned by [`ZeroDisk::new`] when the disk geometry is invalid.
#[derive(Debug, thiserror::Error)]
#[error("disk size {0:#x} is not a multiple of the sector size {1}")]
pub enum InvalidGeometry {
    /// The sector size is invalid.
    #[error("sector size {0} is invalid")]
    InvalidSectorSize(u32),
    /// The disk size is not a multiple of the sector size.
    #[error("disk size {disk_size:#x} is not a multiple of the sector size {sector_size}")]
    NotSectorMultiple {
        /// The disk size.
        disk_size: u64,
        /// The sector size.
        sector_size: u32,
    },
    /// The disk has no sectors.
    #[error("disk has no sectors")]
    EmptyDisk,
}

impl ZeroDisk {
    /// Creates a new disk with the given geometry.
    pub fn new(sector_size: u32, disk_size: u64) -> Result<Self, InvalidGeometry> {
        if !sector_size.is_power_of_two() || sector_size < 512 {
            return Err(InvalidGeometry::InvalidSectorSize(sector_size));
        }
        if disk_size % sector_size as u64 != 0 {
            return Err(InvalidGeometry::NotSectorMultiple {
                disk_size,
                sector_size,
            });
        }
        if disk_size == 0 {
            return Err(InvalidGeometry::EmptyDisk);
        }
        Ok(Self {
            sector_size,
            sector_count: disk_size / sector_size as u64,
        })
    }
}

impl DiskIo for ZeroDisk {
    fn disk_type(&self) -> &str {
        "zero"
    }

    fn sector_count(&self) -> u64 {
        self.sector_count
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        None
    }

    fn physical_sector_size(&self) -> u32 {
        512
    }

    fn is_fua_respected(&self) -> bool {
        true
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        _sector: u64,
    ) -> Result<(), DiskError> {
        buffers.writer().zero(buffers.len()).map_err(Into::into)
    }

    async fn write_vectored(
        &self,
        _buffers: &RequestBuffers<'_>,
        _sector: u64,
        _fua: bool,
    ) -> Result<(), DiskError> {
        Err(DiskError::ReadOnly)
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        Ok(())
    }
}
