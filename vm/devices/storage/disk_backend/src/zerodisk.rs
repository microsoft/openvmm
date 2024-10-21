// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A read-only disk that always returns zero on read.

use super::SimpleDisk;
use crate::AsyncDisk;
use crate::DiskError;
use guestmem::MemoryWrite;
use inspect::Inspect;
use scsi_buffers::RequestBuffers;
use stackfuture::StackFuture;
use std::future::ready;

/// A read-only disk that always returns zero on read.
#[derive(Debug, Inspect)]
pub struct ZeroDisk {
    sector_size: u32,
    sector_count: u64,
}

#[derive(Debug, thiserror::Error)]
#[error("disk size {0:#x} is not a multiple of the sector size {1}")]
pub enum InvalidGeometry {
    #[error("sector size {0} is invalid")]
    InvalidSectorSize(u32),
    #[error("disk size {disk_size:#x} is not a multiple of the sector size {sector_size}")]
    NotSectorMultiple { disk_size: u64, sector_size: u32 },
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

impl SimpleDisk for ZeroDisk {
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
}

impl AsyncDisk for ZeroDisk {
    fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        _sector: u64,
    ) -> StackFuture<'_, Result<(), DiskError>, { crate::ASYNC_DISK_STACK_SIZE }> {
        StackFuture::from(ready(
            buffers.writer().zero(buffers.len()).map_err(Into::into),
        ))
    }

    fn write_vectored(
        &self,
        _buffers: &RequestBuffers<'_>,
        _sector: u64,
        _fua: bool,
    ) -> StackFuture<'_, Result<(), DiskError>, { crate::ASYNC_DISK_STACK_SIZE }> {
        StackFuture::from(ready(Err(DiskError::ReadOnly)))
    }

    fn sync_cache(
        &self,
    ) -> StackFuture<'_, Result<(), DiskError>, { crate::ASYNC_DISK_STACK_SIZE }> {
        StackFuture::from(ready(Ok(())))
    }
}
