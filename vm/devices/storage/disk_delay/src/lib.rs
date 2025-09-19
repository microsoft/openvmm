// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A disk device wrapper that provides configurable storage delay on Read/Write I/O operations to a disk.

#![forbid(unsafe_code)]

/// Provides a disk with a delay on every I/O operation.
pub mod resolver;

use disk_backend::Disk;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::UnmapBehavior;
use inspect::Inspect;
use mesh::Cell;
use pal_async::timer::PolledTimer;
use scsi_buffers::RequestBuffers;
use std::time::Duration;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;

/// A disk with delay on every I/O operation.
#[derive(Inspect)]
pub struct DelayDisk {
    #[inspect(with = "|x| x.get()")]
    delay: Cell<Duration>,
    inner: Disk,
    driver: VmTaskDriver,
}

impl DelayDisk {
    /// Creates a new disk with a specified delay on I/O operations.
    pub fn new(delay: Cell<Duration>, inner: Disk, driver_source: &VmTaskDriverSource) -> Self {
        Self {
            delay,
            inner,
            driver: driver_source.simple(),
        }
    }
}

impl DiskIo for DelayDisk {
    fn disk_type(&self) -> &str {
        "delay"
    }

    /// Passthrough
    fn sector_count(&self) -> u64 {
        self.inner.sector_count()
    }

    /// Passthrough
    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }

    /// Passthrough
    fn disk_id(&self) -> Option<[u8; 16]> {
        self.inner.disk_id()
    }

    /// Passthrough
    fn physical_sector_size(&self) -> u32 {
        self.inner.physical_sector_size()
    }

    /// Passthrough
    fn is_fua_respected(&self) -> bool {
        self.inner.is_fua_respected()
    }

    /// Passthrough
    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }

    /// Passthrough
    fn pr(&self) -> Option<&dyn disk_backend::pr::PersistentReservation> {
        self.inner.pr()
    }

    /// Delay and then Passthrough
    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        PolledTimer::new(&self.driver).sleep(self.delay.get()).await;
        self.inner.read_vectored(buffers, sector).await
    }

    /// Delay and then Passthrough
    async fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        PolledTimer::new(&self.driver).sleep(self.delay.get()).await;
        self.inner.write_vectored(buffers, sector, fua).await
    }

    /// Passthrough
    async fn sync_cache(&self) -> Result<(), DiskError> {
        self.inner.sync_cache().await
    }

    /// Passthrough
    async fn wait_resize(&self, sector_count: u64) -> u64 {
        self.inner.wait_resize(sector_count).await
    }

    /// Passthrough
    fn unmap(
        &self,
        sector: u64,
        count: u64,
        block_level_only: bool,
    ) -> impl Future<Output = Result<(), DiskError>> + Send {
        self.inner.unmap(sector, count, block_level_only)
    }

    /// Passthrough
    fn unmap_behavior(&self) -> UnmapBehavior {
        self.inner.unmap_behavior()
    }

    /// Passthrough
    fn optimal_unmap_sectors(&self) -> u32 {
        self.inner.optimal_unmap_sectors()
    }
}
