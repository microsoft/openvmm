// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VMGS storage implementation on top of [`Disk`].

use disk_backend::Disk;
use disk_backend::DiskError;
use guestmem::GuestMemory;
use scsi_buffers::OwnedRequestBuffers;
use thiserror::Error;

#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
pub(crate) struct VmgsStorage {
    disk: Disk,
    mem: GuestMemory,
    mem_size: usize,
    sector_size: u32,
    sector_shift: u32,
}

/// Error due to underlying VMGS storage.
#[derive(Debug, Error)]
pub enum StorageError {
    /// A disk IO error.
    #[error(transparent)]
    Disk(DiskError),
    /// The access was not aligned to a sector boundary.
    #[error("access is not aligned to a sector boundary")]
    Unaligned,
    /// The storage has an invalid sector count
    #[error("invalid sector count {0}")]
    InvalidSectorCount(u64),
    /// The storage has an invalid sector size
    #[error("invalid sector size {0}")]
    InvalidSectorSize(u32),
}

impl VmgsStorage {
    pub fn new(disk: Disk) -> Self {
        let sector_size = disk.sector_size();
        // Max IO size. Balance between performance and memory usage.
        let mem_size = 64 * 1024;
        Self {
            mem: GuestMemory::allocate(mem_size),
            mem_size,
            sector_size,
            sector_shift: sector_size.trailing_zeros(),
            disk,
        }
    }

    pub fn new_validated(disk: Disk) -> Result<Self, StorageError> {
        let storage = VmgsStorage::new(disk);
        // Errors from validate are fatal, as they involve invalid device metadata
        storage.validate()?;
        Ok(storage)
    }

    pub fn validate(&self) -> Result<(), StorageError> {
        let sector_count = self.sector_count();
        let sector_size = self.sector_size();

        // Don't need to parse MBR/GPT table, VMGS uses RAW file format

        // Validate capacity and max transfer size. This also enesures that there are no arithmetic
        // overflows when converting from sector counts to byte counts.
        if sector_count == 0 || sector_count > u64::MAX / 4096 {
            return Err(StorageError::InvalidSectorCount(sector_count));
        }

        // Any power-of-2 sector size up to 4096 bytes works, but in practice only 512 and 4096
        // indicate a supported (tested) device configuration.
        if sector_size != 512 && sector_size != 4096 {
            return Err(StorageError::InvalidSectorSize(sector_size));
        }

        Ok(())
    }

    /// Read from the block device.
    ///
    /// The beginning of the read must be sector aligned, but the end need not
    /// be.
    pub async fn read_block(
        &self,
        mut byte_offset: u64,
        buf: &mut [u8],
    ) -> Result<(), StorageError> {
        if byte_offset & (self.sector_size as u64 - 1) != 0 {
            return Err(StorageError::Unaligned);
        }
        for buf in buf.chunks_mut(self.mem_size) {
            let sector = byte_offset >> self.sector_shift;
            let sector_end = (byte_offset + buf.len() as u64 + (self.sector_size as u64 - 1))
                >> self.sector_shift;

            let len = (sector_end - sector) << self.sector_shift;
            let buffers = OwnedRequestBuffers::linear(0, len as usize, true);
            self.disk
                .read_vectored(&buffers.buffer(&self.mem), sector)
                .await
                .map_err(StorageError::Disk)?;

            self.mem.read_at(0, buf).unwrap();
            byte_offset += len;
        }
        Ok(())
    }

    /// Write a block to the block device.
    ///
    /// The beginning of the write must be sector aligned, but the end need not
    /// be.
    ///
    /// CAUTION: if the end of the write is not sector aligned, the trailing sector
    /// data will be zeroed out.
    pub async fn write_block(
        &mut self,
        mut byte_offset: u64,
        buf: &[u8],
    ) -> Result<(), StorageError> {
        if byte_offset & (self.sector_size as u64 - 1) != 0 {
            return Err(StorageError::Unaligned);
        }
        for buf in buf.chunks(self.mem_size) {
            let sector = byte_offset >> self.sector_shift;
            let sector_end = (byte_offset + buf.len() as u64 + (self.sector_size as u64 - 1))
                >> self.sector_shift;
            let len = (sector_end - sector) << self.sector_shift;

            self.mem.write_at(0, buf).unwrap();
            // Zero the trailing sector data.
            self.mem
                .fill_at(buf.len() as u64, 0, len as usize - buf.len())
                .unwrap();

            let buffers = OwnedRequestBuffers::linear(0, len as usize, false);
            self.disk
                .write_vectored(&buffers.buffer(&self.mem), sector, false)
                .await
                .map_err(StorageError::Disk)?;
            byte_offset += len;
        }
        Ok(())
    }

    /// Flush any buffered data.
    pub async fn flush(&mut self) -> Result<(), StorageError> {
        self.disk.sync_cache().await.map_err(StorageError::Disk)
    }

    pub fn sector_size(&self) -> u32 {
        self.sector_size
    }

    pub fn sector_count(&self) -> u64 {
        self.disk.sector_count()
    }

    /// Capacity in VMGS blocks.
    pub fn block_capacity(&self) -> u32 {
        ((self.sector_count() * self.sector_size() as u64)
            .min(vmgs_format::VMGS_MAX_CAPACITY_BYTES)
            / vmgs_format::VMGS_BYTES_PER_BLOCK as u64) as u32
    }

    pub fn aligned_header_size(&self) -> u64 {
        assert!(self.sector_size() >= size_of::<vmgs_format::VmgsHeader>() as u32);
        self.sector_size().into()
    }
}
