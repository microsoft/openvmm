// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Defines the [`SimpleDisk`] type, which provides an interface to a block
//! device, used for different disk frontends (such as the floppy disk, IDE,
//! SCSI, or NVMe emulators) as well as direct disk access for other purposes
//! (such as the VMGS file system).
//!
//! `SimpleDisk`s are backed by a [`DiskIo`] implementation. Specific disk
//! backends should be in their own crates. The exceptions that prove the rule
//! is [`ZeroDisk`][], which is small enough to be in this crate and serve as an
//! example.
//!
//! [`ZeroDisk`]: crate::zerodisk::ZeroDisk

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod pr;
pub mod resolve;
pub mod sync_wrapper;
pub mod zerodisk;

use guestmem::AccessError;
use inspect::Inspect;
use scsi_buffers::RequestBuffers;
use stackfuture::StackFuture;
use std::fmt::Debug;
use std::future::ready;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use thiserror::Error;

/// A disk operation error.
#[derive(Debug, Error)]
pub enum DiskError {
    /// The request failed due to a preempt and abort status.
    #[error("aborted command")]
    AbortDueToPreemptAndAbort,
    /// The LBA was out of range.
    #[error("illegal request")]
    IllegalBlock,
    /// The request failed due to invalid input.
    #[error("invalid input")]
    InvalidInput,
    /// The request failed due to an unrecovered IO error.
    #[error("io error")]
    Io(#[source] std::io::Error),
    /// The request failed due to a reportable medium error.
    #[error("medium error")]
    MediumError(#[source] std::io::Error, MediumErrorDetails),
    /// The request failed due to a failure to access the specified buffers.
    #[error("failed to access guest memory")]
    MemoryAccess(#[from] AccessError),
    /// The request failed because the disk is read-only.
    #[error("attempt to write to read-only disk/range")]
    ReadOnly,
    /// The request failed due to a persistent reservation conflict.
    #[error("reservation conflict")]
    ReservationConflict,
    /// The request failed because eject is not supported.
    #[error("unsupported eject")]
    UnsupportedEject,
}

/// Failure details for [`DiskError::MediumError`].
#[derive(Debug)]
pub enum MediumErrorDetails {
    /// The medium had an application tag check failure.
    ApplicationTagCheckFailed,
    /// The medium had a guard check failure.
    GuardCheckFailed,
    /// The medium had a reference tag check failure.
    ReferenceTagCheckFailed,
    /// The medium had an unrecovered read error.
    UnrecoveredReadError,
    /// The medium had a write fault.
    WriteFault,
}

/// Disk metadata and IO operations.
pub trait DiskIo: 'static + Send + Sync + Inspect {
    /// Returns the disk type name as a string.
    ///
    /// This is used for diagnostic purposes.
    fn disk_type(&self) -> &str;

    /// Returns the current sector count.
    ///
    /// For some backing stores, this may change at runtime. If it does, then
    /// the backing store must also implement [`AsyncDisk::wait_resize`].
    fn sector_count(&self) -> u64;

    /// Returns the logical sector size of the backing store.
    ///
    /// This must not change at runtime.
    fn sector_size(&self) -> u32;

    /// Optionally returns a 16-byte identifier for the disk, if there is a
    /// natural one for this backing store.
    ///
    /// This may be exposed to the guest as a unique disk identifier.
    /// This must not change at runtime.
    fn disk_id(&self) -> Option<[u8; 16]>;

    /// Returns the physical sector size of the backing store.
    ///
    /// This must not change at runtime.
    fn physical_sector_size(&self) -> u32;

    /// Returns true if the `fua` parameter to [`AsyncDisk::write_vectored`] is
    /// respected by the backing store by ensuring that the IO is immediately
    /// committed to disk.
    fn is_fua_respected(&self) -> bool;

    /// Returns true if the disk is read only.
    fn is_read_only(&self) -> bool;

    /// Optionally returns a trait object to issue unmap (trim/discard)
    /// requests.
    fn unmap(&self) -> Option<impl Unmap> {
        None::<NoUnmap>
    }

    /// Optionally returns a trait object to issue get LBA status requests.
    fn lba_status(&self) -> Option<&dyn GetLbaStatus> {
        None
    }

    /// Optionally returns a trait object to issue persistent reservation
    /// requests.
    fn pr(&self) -> Option<&dyn pr::PersistentReservation> {
        None
    }

    /// Issues an asynchronous eject media operation to the disk.
    fn eject(&self) -> impl Future<Output = Result<(), DiskError>> + Send {
        ready(Err(DiskError::UnsupportedEject))
    }

    /// Issues an asynchronous read-scatter operation to the disk.
    ///
    /// # Arguments
    /// * `buffers` - An object representing the data buffers into which the disk data will be transferred.
    /// * `sector` - The logical sector at which the read operation starts.
    fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> impl Future<Output = Result<(), DiskError>> + Send;

    /// Issues an asynchronous write-gather operation to the disk.
    /// # Arguments
    /// * `buffers` - An object representing the data buffers containing the data to transfer to the disk.
    /// * `sector` - The logical sector at which the write operation starts.
    /// * `fua` - A flag indicates if FUA (force unit access) is requested.
    fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> impl Future<Output = Result<(), DiskError>> + Send;

    /// Issues an asynchronous flush operation to the disk.
    fn sync_cache(&self) -> impl Future<Output = Result<(), DiskError>> + Send;

    /// Waits for the disk sector size to be different than the specified value.
    fn wait_resize(&self, sector_count: u64) -> impl Future<Output = u64> + Send {
        let _ = sector_count;
        std::future::pending()
    }
}

/// An asynchronous block device.
///
/// This type is cheap to clone, for sharing the disk among multiple concurrent
/// users.
#[derive(Inspect, Clone)]
#[inspect(extra = "Self::inspect_extra")]
pub struct SimpleDisk(#[inspect(flatten)] Arc<SimpleDiskInner>);

impl SimpleDisk {
    fn inspect_extra(&self, resp: &mut inspect::Response<'_>) {
        resp.field("disk_type", self.0.disk.disk_type())
            .field("sector_count", self.0.disk.sector_count())
            .field("supports_lba_status", self.0.disk.lba_status().is_some())
            .field("supports_pr", self.0.disk.pr().is_some())
            .field(
                "optimal_unmap_sectors",
                self.unmap().map(|u| u.optimal_unmap_sectors()),
            );
    }
}

impl Debug for SimpleDisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SimpleDisk").finish()
    }
}

#[derive(Inspect)]
#[inspect(bound = "T: DynSimpleDisk")]
struct SimpleDiskInner<T: ?Sized = dyn DynSimpleDisk> {
    sector_size: u32,
    sector_shift: u32,
    physical_sector_size: u32,
    disk_id: Option<[u8; 16]>,
    is_fua_respected: bool,
    is_read_only: bool,
    supports_unmap: bool,
    disk: T,
}

/// Errors that can occur when creating a `SimpleDisk`.
#[derive(Debug, Error)]
pub enum InvalidDisk {
    /// The sector size is invalid.
    #[error("invalid sector size: {0}")]
    InvalidSectorSize(u32),
    /// The physical sector size is invalid.
    #[error("invalid physical sector size: {0}")]
    InvalidPhysicalSectorSize(u32),
}

impl SimpleDisk {
    /// Returns a new `SimpleDisk` wrapping the given backing disk.
    pub fn new(disk: impl 'static + DiskIo) -> Result<Self, InvalidDisk> {
        // Cache the metadata locally to validate it and so that it can be
        // accessed without needing to go through the trait object. This is more
        // efficient and ensures the backing disk does not change these values
        // during the lifetime of the disk.
        let sector_size = disk.sector_size();
        if !sector_size.is_power_of_two() || sector_size < 512 {
            return Err(InvalidDisk::InvalidSectorSize(sector_size));
        }
        let physical_sector_size = disk.physical_sector_size();
        if !physical_sector_size.is_power_of_two() || physical_sector_size < sector_size {
            return Err(InvalidDisk::InvalidPhysicalSectorSize(physical_sector_size));
        }
        let supports_unmap = disk.unmap().is_some();
        Ok(Self(Arc::new(SimpleDiskInner {
            supports_unmap,
            sector_size,
            sector_shift: sector_size.trailing_zeros(),
            physical_sector_size,
            disk_id: disk.disk_id(),
            is_fua_respected: disk.is_fua_respected(),
            is_read_only: disk.is_read_only(),
            disk,
        })))
    }

    /// Returns the current sector count.
    ///
    /// For some backing stores, this may change at runtime. If it does, then
    /// the backing store must also implement [`AsyncDisk::wait_resize`].
    pub fn sector_count(&self) -> u64 {
        self.0.disk.sector_count()
    }

    /// Returns the logical sector size of the backing store.
    pub fn sector_size(&self) -> u32 {
        self.0.sector_size
    }

    /// Returns log2 of the logical sector size of the backing store.
    pub fn sector_shift(&self) -> u32 {
        self.0.sector_shift
    }

    /// Optionally returns a 16-byte identifier for the disk, if there is a
    /// natural one for this backing store.
    ///
    /// This may be exposed to the guest as a unique disk identifier.
    pub fn disk_id(&self) -> Option<[u8; 16]> {
        self.0.disk_id
    }

    /// Returns the physical sector size of the backing store.
    pub fn physical_sector_size(&self) -> u32 {
        self.0.physical_sector_size
    }

    /// Returns true if the `fua` parameter to [`AsyncDisk::write_vectored`] is
    /// respected by the backing store by ensuring that the IO is immediately
    /// committed to disk.
    pub fn is_fua_respected(&self) -> bool {
        self.0.is_fua_respected
    }

    /// Returns true if the disk is read only.
    pub fn is_read_only(&self) -> bool {
        self.0.is_read_only
    }

    /// Optionally returns a trait object to issue unmap (trim/discard)
    /// requests.
    pub fn unmap(&self) -> Option<SimpleDiskUnmap<'_>> {
        if self.0.supports_unmap {
            Some(SimpleDiskUnmap(&self.0))
        } else {
            None
        }
    }

    /// Optionally returns a trait object to issue get LBA status requests.
    pub fn lba_status(&self) -> Option<&dyn GetLbaStatus> {
        self.0.disk.lba_status()
    }

    /// Optionally returns a trait object to issue persistent reservation
    /// requests.
    pub fn pr(&self) -> Option<&dyn pr::PersistentReservation> {
        self.0.disk.pr()
    }

    /// Issues an asynchronous eject media operation to the disk.
    pub fn eject(&self) -> impl '_ + Future<Output = Result<(), DiskError>> {
        self.0.disk.eject()
    }

    /// Issues an asynchronous read-scatter operation to the disk.
    ///
    /// # Arguments
    /// * `buffers` - An object representing the data buffers into which the disk data will be transferred.
    /// * `sector` - The logical sector at which the read operation starts.
    pub async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        self.0.disk.read_vectored(buffers, sector).await
    }

    /// Issues an asynchronous write-gather operation to the disk.
    /// # Arguments
    /// * `buffers` - An object representing the data buffers containing the data to transfer to the disk.
    /// * `sector` - The logical sector at which the write operation starts.
    /// * `fua` - A flag indicates if FUA (force unit access) is requested.
    pub async fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        self.0.disk.write_vectored(buffers, sector, fua).await
    }

    /// Issues an asynchronous flush operation to the disk.
    pub async fn sync_cache(&self) -> Result<(), DiskError> {
        self.0.disk.sync_cache().await
    }

    /// Waits for the disk sector size to be different than the specified value.
    pub async fn wait_resize(&self, sector_count: u64) -> u64 {
        self.0.disk.wait_resize(sector_count).await
    }
}

/// Access to a disk's unmap operations. Returned by [`SimpleDisk::unmap`].
pub struct SimpleDiskUnmap<'a>(&'a SimpleDiskInner);

impl Unmap for SimpleDiskUnmap<'_> {
    async fn unmap(
        &self,
        sector_offset: u64,
        sector_count: u64,
        block_level_only: bool,
    ) -> Result<(), DiskError> {
        self.0
            .disk
            .unmap(sector_offset, sector_count, block_level_only)
            .await
    }

    fn optimal_unmap_sectors(&self) -> u32 {
        self.0.disk.optimal_unmap_sectors()
    }
}

/// A trait to get LBA status and block index information.
pub trait GetLbaStatus {
    /// Returns the block index information for the given file offset.
    fn file_offset_to_device_block_index_and_length(
        &self,
        disk: &SimpleDisk,
        _start_offset: u64,
        _get_lba_status_range_length: u64,
        _block_size: u64,
    ) -> DeviceBlockIndexInfo {
        let sector_size = disk.sector_size() as u64;
        let sector_count = disk.sector_count();
        let disk_size = sector_size * sector_count;

        // Treat fully allocation disk or fixed disk as one large block and just return
        // enough descriptors from the LBA requested till the last LBA on disk.
        //
        // LbaPerBlock is a ULONG and technically with MAXULONG * 512 byte sectors,
        // we can get upto 1.99 TB. The LBA descriptor also holds a ULONG
        // LogicalBlockCount and can have an issue for larger than 2TB disks.
        let lba_per_block = std::cmp::min(sector_count, u32::MAX.into());
        let block_size_large = lba_per_block * sector_size;
        let block_count = ((disk_size + block_size_large - 1) / block_size_large) as u32;
        DeviceBlockIndexInfo {
            first_partial_block_size: 0,
            first_full_block_index: 0,
            block_count,
            last_partial_block_size: 0,
            lba_per_block,
        }
    }

    /// Returns the LBA status for the given block number.
    fn get_block_lba_status(
        &self,
        _block_number: u32,
        _leaf_node_state_only: bool,
    ) -> Result<LbaStatus, DiskError> {
        Ok(LbaStatus::Mapped)
    }
}

/// The LBA status of a block.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LbaStatus {
    /// The block is mapped.
    Mapped,
    /// The block is deallocated.
    Deallocated,
    /// The block is anchored.
    Anchored,
}

/// Result of a get LBA status request.
#[derive(Debug, Default, Copy, Clone)]
pub struct DeviceBlockIndexInfo {
    /// The size of the first partial block.
    pub first_partial_block_size: u32,
    /// The index of the first full block.
    pub first_full_block_index: u32,
    /// The number of blocks.
    pub block_count: u32,
    /// The size of the last partial block.
    pub last_partial_block_size: u32,
    /// The number of LBAs per block.
    pub lba_per_block: u64,
}

/// Unmap disk sectors that are no longer in use.
pub trait Unmap: Send + Sync {
    /// Unmaps the specified sectors.
    fn unmap(
        &self,
        sector_offset: u64,
        sector_count: u64,
        block_level_only: bool,
    ) -> impl Future<Output = Result<(), DiskError>> + Send;

    /// Returns the optimal number of sectors to unmap in a single operation.
    fn optimal_unmap_sectors(&self) -> u32;
}

impl<T: Unmap> Unmap for &T {
    fn unmap(
        &self,
        sector_offset: u64,
        sector_count: u64,
        block_level_only: bool,
    ) -> impl Future<Output = Result<(), DiskError>> {
        (*self).unmap(sector_offset, sector_count, block_level_only)
    }

    fn optimal_unmap_sectors(&self) -> u32 {
        (*self).optimal_unmap_sectors()
    }
}

struct NoUnmap;

impl Unmap for NoUnmap {
    async fn unmap(
        &self,
        _sector_offset: u64,
        _sector_count: u64,
        _block_level_only: bool,
    ) -> Result<(), DiskError> {
        unreachable!()
    }

    fn optimal_unmap_sectors(&self) -> u32 {
        unreachable!()
    }
}

/// The amount of space reserved for a DiskIo future
///
/// This was chosen by running `cargo test -p storvsp -- --no-capture` and looking at the required
/// size that was given in the failure message
const ASYNC_DISK_STACK_SIZE: usize = 1256;

trait DynSimpleDisk: Send + Sync + Inspect {
    fn disk_type(&self) -> &str;
    fn sector_count(&self) -> u64;

    fn unmap(
        &self,
        sector_offset: u64,
        sector_count: u64,
        block_level_only: bool,
    ) -> StackFuture<'_, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }>;

    fn optimal_unmap_sectors(&self) -> u32;
    fn lba_status(&self) -> Option<&dyn GetLbaStatus>;
    fn pr(&self) -> Option<&dyn pr::PersistentReservation>;
    fn eject(&self) -> StackFuture<'_, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }>;

    fn read_vectored<'a>(
        &'a self,
        buffers: &'a RequestBuffers<'a>,
        sector: u64,
    ) -> StackFuture<'a, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }>;

    fn write_vectored<'a>(
        &'a self,
        buffers: &'a RequestBuffers<'a>,
        sector: u64,
        fua: bool,
    ) -> StackFuture<'a, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }>;

    fn sync_cache(&self) -> StackFuture<'_, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }>;

    fn wait_resize<'a>(
        &'a self,
        sector_count: u64,
    ) -> Pin<Box<dyn 'a + Send + Future<Output = u64>>> {
        let _ = sector_count;
        Box::pin(std::future::pending())
    }
}

impl<T: DiskIo> DynSimpleDisk for T {
    fn disk_type(&self) -> &str {
        self.disk_type()
    }

    fn sector_count(&self) -> u64 {
        self.sector_count()
    }

    fn unmap(
        &self,
        sector_offset: u64,
        sector_count: u64,
        block_level_only: bool,
    ) -> StackFuture<'_, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }> {
        StackFuture::from_or_box(async move {
            self.unmap()
                .unwrap()
                .unmap(sector_offset, sector_count, block_level_only)
                .await
        })
    }

    fn optimal_unmap_sectors(&self) -> u32 {
        self.unmap().unwrap().optimal_unmap_sectors()
    }

    fn lba_status(&self) -> Option<&dyn GetLbaStatus> {
        self.lba_status()
    }

    fn pr(&self) -> Option<&dyn pr::PersistentReservation> {
        self.pr()
    }

    fn eject(&self) -> StackFuture<'_, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }> {
        StackFuture::from_or_box(self.eject())
    }

    fn read_vectored<'a>(
        &'a self,
        buffers: &'a RequestBuffers<'a>,
        sector: u64,
    ) -> StackFuture<'a, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }> {
        StackFuture::from_or_box(self.read_vectored(buffers, sector))
    }

    fn write_vectored<'a>(
        &'a self,
        buffers: &'a RequestBuffers<'a>,
        sector: u64,
        fua: bool,
    ) -> StackFuture<'a, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }> {
        StackFuture::from_or_box(self.write_vectored(buffers, sector, fua))
    }

    fn sync_cache(&self) -> StackFuture<'_, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }> {
        StackFuture::from_or_box(self.sync_cache())
    }
}
