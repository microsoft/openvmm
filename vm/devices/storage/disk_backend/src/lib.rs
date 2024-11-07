// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Definitions for [`SimpleDisk`] and related traits, which are used to define
//! disk backends that work with different disk frontends (such as the Floppy,
//! IDE, SCSI, or NVMe emulators).
//!
//! Specific disk backends should be in their own crates. The exceptions that
//! prove the rule is [`ZeroDisk`][], which is small enough to be in this crate
//! and serve as an example.
//!
//! [`ZeroDisk`]: crate::zerodisk::ZeroDisk

#![forbid(unsafe_code)]

pub mod pr;
pub mod resolve;
pub mod sync_wrapper;
pub mod zerodisk;

use guestmem::AccessError;
use hvdef::HvError;
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
    #[error("aborted command")]
    AbortDueToPreemptAndAbort,
    #[error("illegal request")]
    IllegalBlock,
    #[error("invalid input")]
    InvalidInput,
    #[error("io error")]
    Io(#[source] std::io::Error),
    #[error("medium error")]
    MediumError(#[source] std::io::Error, MediumErrorDetails),
    #[error("failed to access guest memory")]
    MemoryAccess(#[from] AccessError),
    #[error("attempt to write to read-only disk/range")]
    ReadOnly,
    #[error("reservation conflict")]
    ReservationConflict,
    #[error("unsupported eject")]
    UnsupportedEject,
    #[error("failed to pin/unpin guest memory {0}")]
    Hv(HvError),
}

/// Io error details
#[derive(Debug)]
pub enum MediumErrorDetails {
    ApplicationTagCheckFailed,
    GuardCheckFailed,
    ReferenceTagCheckFailed,
    UnrecoveredReadError,
    WriteFault,
}

pub trait SimpleDisk: Send + Sync + Inspect + AsyncDisk {
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
    fn unmap(&self) -> Option<&dyn Unmap> {
        None
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
    fn eject(&self) -> StackFuture<'_, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }> {
        StackFuture::from(ready(Err(DiskError::UnsupportedEject)))
    }
}

impl SimpleDisk for Arc<dyn SimpleDisk> {
    fn disk_type(&self) -> &str {
        self.as_ref().disk_type()
    }

    fn sector_count(&self) -> u64 {
        self.as_ref().sector_count()
    }

    fn sector_size(&self) -> u32 {
        self.as_ref().sector_size()
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        self.as_ref().disk_id()
    }

    fn physical_sector_size(&self) -> u32 {
        self.as_ref().physical_sector_size()
    }

    fn is_fua_respected(&self) -> bool {
        self.as_ref().is_fua_respected()
    }

    fn is_read_only(&self) -> bool {
        self.as_ref().is_read_only()
    }

    fn unmap(&self) -> Option<&dyn Unmap> {
        self.as_ref().unmap()
    }

    fn lba_status(&self) -> Option<&dyn GetLbaStatus> {
        self.as_ref().lba_status()
    }

    fn pr(&self) -> Option<&dyn pr::PersistentReservation> {
        self.as_ref().pr()
    }

    fn eject(&self) -> StackFuture<'_, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }> {
        self.as_ref().eject()
    }
}

impl<T: SimpleDisk + ?Sized> SimpleDisk for &T {
    fn disk_type(&self) -> &str {
        (*self).disk_type()
    }

    fn sector_count(&self) -> u64 {
        (*self).sector_count()
    }

    fn sector_size(&self) -> u32 {
        (*self).sector_size()
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        (*self).disk_id()
    }

    fn physical_sector_size(&self) -> u32 {
        (*self).physical_sector_size()
    }

    fn is_fua_respected(&self) -> bool {
        (*self).is_fua_respected()
    }

    fn is_read_only(&self) -> bool {
        (*self).is_read_only()
    }

    fn unmap(&self) -> Option<&dyn Unmap> {
        (*self).unmap()
    }

    fn lba_status(&self) -> Option<&dyn GetLbaStatus> {
        (*self).lba_status()
    }

    fn pr(&self) -> Option<&dyn pr::PersistentReservation> {
        (*self).pr()
    }

    fn eject(&self) -> StackFuture<'_, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }> {
        (*self).eject()
    }
}

/// The amount of space reserved for an AsyncDisk future
///
/// This was chosen by running `cargo test -p storvsp -- --no-capture` and looking at the required
/// size that was given in the failure message
pub const ASYNC_DISK_STACK_SIZE: usize = 1256;
pub trait AsyncDisk: Send + Sync {
    /// Issues an asynchronous read-scatter operation to the disk.
    ///
    /// # Arguments
    /// * `buffers` - An object representing the data buffers into which the disk data will be transferred.
    /// * `sector` - The logical sector at which the read operation starts.
    fn read_vectored<'a>(
        &'a self,
        buffers: &'a RequestBuffers<'a>,
        sector: u64,
    ) -> StackFuture<'a, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }>;

    /// Issues an asynchronous write-gather operation to the disk.
    /// # Arguments
    /// * `buffers` - An object representing the data buffers containing the data to transfer to the disk.
    /// * `sector` - The logical sector at which the write operation starts.
    /// * `fua` - A flag indicates if FUA (force unit access) is requested.
    fn write_vectored<'a>(
        &'a self,
        buffers: &'a RequestBuffers<'a>,
        sector: u64,
        fua: bool,
    ) -> StackFuture<'a, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }>;

    /// Issues an asynchronous flush operation to the disk.
    fn sync_cache(&self) -> StackFuture<'_, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }>;

    /// Waits for the disk sector size to be different than the specified value.
    fn wait_resize<'a>(
        &'a self,
        sector_count: u64,
    ) -> Pin<Box<dyn 'a + Send + Future<Output = u64>>> {
        let _ = sector_count;
        Box::pin(std::future::pending())
    }
}

impl AsyncDisk for Arc<dyn SimpleDisk> {
    fn read_vectored<'a>(
        &'a self,
        buffers: &'a RequestBuffers<'a>,
        sector: u64,
    ) -> StackFuture<'a, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }> {
        self.as_ref().read_vectored(buffers, sector)
    }

    fn write_vectored<'a>(
        &'a self,
        buffers: &'a RequestBuffers<'a>,
        sector: u64,
        fua: bool,
    ) -> StackFuture<'a, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }> {
        self.as_ref().write_vectored(buffers, sector, fua)
    }

    fn sync_cache(&self) -> StackFuture<'_, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }> {
        self.as_ref().sync_cache()
    }

    fn wait_resize<'a>(
        &'a self,
        sector_count: u64,
    ) -> Pin<Box<dyn 'a + Send + Future<Output = u64>>> {
        self.as_ref().wait_resize(sector_count)
    }
}

impl<T: SimpleDisk + ?Sized> AsyncDisk for &T {
    fn read_vectored<'a>(
        &'a self,
        buffers: &'a RequestBuffers<'a>,
        sector: u64,
    ) -> StackFuture<'a, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }> {
        (*self).read_vectored(buffers, sector)
    }

    fn write_vectored<'a>(
        &'a self,
        buffers: &'a RequestBuffers<'a>,
        sector: u64,
        fua: bool,
    ) -> StackFuture<'a, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }> {
        (*self).write_vectored(buffers, sector, fua)
    }

    fn sync_cache(&self) -> StackFuture<'_, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }> {
        (*self).sync_cache()
    }

    fn wait_resize<'a>(
        &'a self,
        sector_count: u64,
    ) -> Pin<Box<dyn 'a + Send + Future<Output = u64>>> {
        (*self).wait_resize(sector_count)
    }
}

pub trait GetLbaStatus {
    // Default implementation for fully allocated disk or fixed disk
    fn file_offset_to_device_block_index_and_length(
        &self,
        disk: &dyn SimpleDisk,
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

    // Default implementation for fully allocated disk or fixed disk
    fn get_block_lba_status(
        &self,
        _block_number: u32,
        _leaf_node_state_only: bool,
    ) -> Result<LbaStatus, DiskError> {
        Ok(LbaStatus::Mapped)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LbaStatus {
    Mapped,
    Deallocated,
    Anchored,
}

#[derive(Debug, Default, Copy, Clone)]
pub struct DeviceBlockIndexInfo {
    pub first_partial_block_size: u32,
    pub first_full_block_index: u32,
    pub block_count: u32,
    pub last_partial_block_size: u32,
    pub lba_per_block: u64,
}

pub trait Unmap: Sync {
    fn unmap(
        &self,
        sector_offset: u64,
        sector_count: u64,
        block_level_only: bool,
    ) -> StackFuture<'_, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }>;

    fn optimal_unmap_sectors(&self) -> u32;
}
