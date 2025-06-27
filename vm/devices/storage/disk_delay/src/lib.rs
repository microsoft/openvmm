// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A disk device wrapper that provides configurable storage delay on I/O operations.

#![forbid(unsafe_code)]

mod resolver;

use async_trait::async_trait;
use disk_backend::Disk;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::UnmapBehavior;
use inspect::Inspect;
use scsi_buffers::RequestBuffers;

/// A disk with delay on every I/O operation.
#[derive(Inspect)]
pub struct DelayDisk {
    delay: u64,
    inner: Disk,
}

impl DelayDisk {
    /// Creates a new disk with a specified delay on I/O operations.
    pub fn new(
        delay: u64,
        inner: Disk,
    ) -> Self {
        Self {
            delay,
            inner,
        }
    }
}

impl DiskIo for DelayDisk {
    fn disk_type(&self) -> &str {
        "delay"
    }

    fn sector_count(&self) -> u64 {
        self.inner.sector_count()
    }

    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        self.inner.disk_id()
    }

    fn physical_sector_size(&self) -> u32 {
        self.inner.physical_sector_size()
    }

    fn is_fua_respected(&self) -> bool {
        self.inner.is_fua_respected()
    }

    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }

    /// Optionally returns a trait object to issue persistent reservation
    /// requests.
    fn pr(&self) -> Option<&dyn disk_backend::pr::PersistentReservation> {
        self.inner.pr()
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        // Introduce a delay before reading the data.
        std::thread::sleep(std::time::Duration::from_millis(self.delay));
        self.inner.read_vectored(buffers, sector).await
    }

    async fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        // Write the encrypted data.
        std::thread::sleep(std::time::Duration::from_millis(self.delay));
        self.inner
            .write_vectored(buffers, sector, fua)
            .await
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        self.inner.sync_cache().await
    }

    /// Waits for the disk sector size to be different than the specified value.
    async fn wait_resize(&self, sector_count: u64) -> u64 {
        self.inner.wait_resize(sector_count).await
    }

    fn unmap(
        &self,
        sector: u64,
        count: u64,
        block_level_only: bool,
    ) -> impl std::future::Future<Output = Result<(), DiskError>> + Send {
        self.inner.unmap(sector, count, block_level_only)
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        self.inner.unmap_behavior()
    }

    fn optimal_unmap_sectors(&self) -> u32 {
        self.inner.optimal_unmap_sectors()
    }
}