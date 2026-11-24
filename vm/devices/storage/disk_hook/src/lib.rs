// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Disk decorators with operation hooks for storage tests.
//!
//! [`HookDisk::new`] provides sparse RAM storage using [`disklayer_ram`], and
//! [`HookDisk::wrap`] decorates an existing [`Disk`] while preserving its
//! capabilities.
//!
//! Use [`HookDisk::with_sync_hook`] to observe operation parameters or inject
//! errors, and [`HookDisk::with_async_hook`] to asynchronously gate I/O. Hooks
//! run before the operation; returning an error prevents it from reaching
//! storage. Hooks do not receive data buffers.
//!
//! ```
//! use disk_hook::{Disk, HookDisk};
//!
//! let disk: Disk = HookDisk::new(4096, 512).into_disk();
//! assert_eq!(disk.sector_count(), 8);
//! ```
//!
//! Use this crate as a dev-dependency, not as a production storage backend.

#![forbid(unsafe_code)]

pub use disk_backend::Disk;
pub use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::UnmapBehavior;
use disklayer_ram::ram_disk_with_sector_size;
use inspect::Inspect;
use scsi_buffers::RequestBuffers;
use std::future::Future;
use std::pin::Pin;

/// An operation about to be issued to a test disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskOperation {
    /// Read `len` bytes starting at `sector`.
    Read {
        /// Starting logical sector.
        sector: u64,
        /// Transfer length in bytes.
        len: usize,
    },
    /// Write `len` bytes starting at `sector`, with the requested FUA flag.
    Write {
        /// Starting logical sector.
        sector: u64,
        /// Transfer length in bytes.
        len: usize,
        /// Whether force unit access was requested.
        fua: bool,
    },
    /// Flush the disk's cache.
    SyncCache,
    /// Unmap a range of sectors.
    Unmap {
        /// Starting logical sector.
        sector: u64,
        /// Number of logical sectors.
        count: u64,
        /// Whether only whole allocation blocks should be unmapped.
        block_level_only: bool,
    },
    /// Eject the medium.
    Eject,
}

type HookFuture = Pin<Box<dyn Future<Output = Result<(), DiskError>> + Send>>;
type Hook = dyn Fn(DiskOperation) -> HookFuture + Send + Sync;

/// A RAM disk or disk decorator with an optional operation hook.
///
/// Configure metadata and hooks before calling [`into_disk`](Self::into_disk).
/// The resulting disk handle is cheap to clone and shares storage, capacity,
/// and hook state.
#[derive(Inspect)]
pub struct HookDisk {
    sector_size: u32,
    physical_sector_size: u32,
    disk_id: Option<[u8; 16]>,
    read_only: bool,
    fua_respected: bool,
    unmap_behavior: UnmapBehavior,
    optimal_unmap_sectors: u32,
    disk: Disk,
    #[inspect(skip)]
    hook: Option<Box<Hook>>,
}

impl HookDisk {
    /// Creates a sparse RAM disk of `size` bytes using [`disklayer_ram`].
    ///
    /// Unwritten sectors read as zeroes. Storage is allocated only on writes.
    ///
    /// Panics if the size is zero, the sector size is not a power of two
    /// >= 512, or the size is not a multiple of the sector size.
    pub fn new(size: u64, sector_size: u32) -> Self {
        Self::wrap(
            ram_disk_with_sector_size(size, false, sector_size)
                .expect("invalid hook disk specification"),
        )
    }

    /// Wraps a disk, preserving its metadata, reservations, and resize support.
    pub fn wrap(disk: Disk) -> Self {
        Self {
            sector_size: disk.sector_size(),
            physical_sector_size: disk.physical_sector_size(),
            disk_id: disk.disk_id(),
            read_only: disk.is_read_only(),
            fua_respected: disk.is_fua_respected(),
            unmap_behavior: disk.unmap_behavior(),
            optimal_unmap_sectors: disk.optimal_unmap_sectors(),
            disk,
            hook: None,
        }
    }

    /// Converts this builder into a disk handle, preserving hooks and metadata.
    ///
    /// The returned handle can be cheaply cloned to share storage and hook state.
    ///
    /// Panics if the configured physical sector size is not a power of two or
    /// is smaller than the logical sector size.
    pub fn into_disk(self) -> Disk {
        Disk::new(self).expect("invalid hook disk metadata")
    }

    /// Sets the physical sector size reported by the disk.
    pub fn with_physical_sector_size(mut self, sector_size: u32) -> Self {
        self.physical_sector_size = sector_size;
        self
    }

    /// Sets the identifier reported by the disk.
    pub fn with_disk_id(mut self, disk_id: [u8; 16]) -> Self {
        self.disk_id = Some(disk_id);
        self
    }

    /// Sets whether the disk rejects writes and unmaps that reach the backend.
    ///
    /// Setting this to false does not make a read-only backing disk writable.
    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Sets whether the disk advertises FUA support.
    pub fn with_fua_respected(mut self, respected: bool) -> Self {
        self.fua_respected = respected;
        self
    }

    /// Sets the advertised unmap behavior.
    ///
    /// This does not change the backing disk's behavior. Advertise `Zeroes`
    /// only if it actually zeroes unmapped sectors. [`Disk`] does not forward
    /// ignored unmaps, so they do not invoke the hook.
    pub fn with_unmap_behavior(mut self, behavior: UnmapBehavior) -> Self {
        self.unmap_behavior = behavior;
        self
    }

    /// Sets the advertised optimal unmap granularity in sectors.
    pub fn with_optimal_unmap_sectors(mut self, sectors: u32) -> Self {
        self.optimal_unmap_sectors = sectors;
        self
    }

    /// Installs a synchronous hook, replacing any existing hook.
    ///
    /// The hook runs before each I/O operation and may fail it. Concurrent
    /// operations may invoke the hook concurrently.
    pub fn with_sync_hook<F>(mut self, hook: F) -> Self
    where
        F: Fn(DiskOperation) -> Result<(), DiskError> + Send + Sync + 'static,
    {
        self.hook = Some(Box::new(move |op| Box::pin(std::future::ready(hook(op)))));
        self
    }

    /// Installs an async hook, replacing any existing hook.
    ///
    /// The hook runs before each I/O operation and may fail or suspend it.
    /// Concurrent operations may invoke the hook concurrently.
    pub fn with_async_hook<F, Fut>(mut self, hook: F) -> Self
    where
        F: Fn(DiskOperation) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), DiskError>> + Send + 'static,
    {
        self.hook = Some(Box::new(move |op| Box::pin(hook(op))));
        self
    }

    async fn before(&self, operation: DiskOperation) -> Result<(), DiskError> {
        if let Some(hook) = &self.hook {
            hook(operation).await?;
        }
        Ok(())
    }
}

impl DiskIo for HookDisk {
    fn disk_type(&self) -> &str {
        "hook"
    }

    fn sector_count(&self) -> u64 {
        self.disk.sector_count()
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn physical_sector_size(&self) -> u32 {
        self.physical_sector_size
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        self.disk_id
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn is_fua_respected(&self) -> bool {
        self.fua_respected
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        self.unmap_behavior
    }

    fn optimal_unmap_sectors(&self) -> u32 {
        self.optimal_unmap_sectors
    }

    fn pr(&self) -> Option<&dyn disk_backend::pr::PersistentReservation> {
        self.disk.pr()
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        self.before(DiskOperation::Read {
            sector,
            len: buffers.len(),
        })
        .await?;
        self.disk.read_vectored(buffers, sector).await
    }

    async fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        self.before(DiskOperation::Write {
            sector,
            len: buffers.len(),
            fua,
        })
        .await?;
        if self.read_only {
            return Err(DiskError::ReadOnly);
        }
        self.disk.write_vectored(buffers, sector, fua).await
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        self.before(DiskOperation::SyncCache).await?;
        self.disk.sync_cache().await
    }

    async fn unmap(
        &self,
        sector: u64,
        count: u64,
        block_level_only: bool,
    ) -> Result<(), DiskError> {
        self.before(DiskOperation::Unmap {
            sector,
            count,
            block_level_only,
        })
        .await?;
        if self.read_only {
            return Err(DiskError::ReadOnly);
        }
        self.disk.unmap(sector, count, block_level_only).await
    }

    async fn eject(&self) -> Result<(), DiskError> {
        self.before(DiskOperation::Eject).await?;
        self.disk.eject().await
    }

    async fn wait_resize(&self, sector_count: u64) -> u64 {
        self.disk.wait_resize(sector_count).await
    }
}

#[cfg(test)]
mod tests;
