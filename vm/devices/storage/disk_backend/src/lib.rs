// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The shared disk backend abstraction for OpenVMM storage.
//!
//! This crate defines [`Disk`] and the [`DiskIo`] trait, the central
//! interface between storage frontends (NVMe, SCSI/StorVSP, IDE) and disk
//! backends (host files, block devices, remote blobs, and more).
//!
//! # Architecture
//!
//! Every disk backend implements [`DiskIo`]. Frontends don't interact with
//! backends directly — they hold a [`Disk`], which wraps a type-erased
//! backend (`DynDisk`, an adapter around [`DiskIo`] that normalizes return
//! futures) behind an `Arc` for cheap, concurrent cloning. The `Disk`
//! wrapper caches immutable metadata (sector size, physical sector size,
//! disk ID, FUA support) at construction time and validates that sector
//! sizes are powers of two and at least 512 bytes.
//!
//! # I/O model
//!
//! All I/O is **async** and uses **scatter-gather** buffers via
//! [`RequestBuffers`]. Callers must pass
//! buffers that are an integral number of sectors.
//!
//! The key operations are:
//!
//! - [`DiskIo::read_vectored`] / [`DiskIo::write_vectored`] — async
//!   scatter-gather read and write. The `fua` parameter on writes requests
//!   Force Unit Access (write-through to stable storage). Whether FUA is
//!   actually respected depends on the backend — check
//!   [`DiskIo::is_fua_respected`].
//! - [`DiskIo::sync_cache`] — flush (equivalent to SCSI SYNCHRONIZE CACHE
//!   or NVMe FLUSH).
//! - [`DiskIo::unmap`] — trim / deallocate sectors. The
//!   [`DiskIo::unmap_behavior`] method reports whether unmapped sectors
//!   become zero, become indeterminate, or whether unmap is ignored
//!   entirely.
//! - [`DiskIo::eject`] — eject media (optical drives only). The default
//!   returns [`DiskError::UnsupportedEject`]. Eject is a media state change
//!   managed by the SCSI DVD layer, not by the backend.
//! - [`DiskIo::wait_resize`] — block until the disk's sector count changes.
//!   The default returns [`std::future::pending()`], meaning the backend
//!   never signals a resize. Only backends that can detect runtime capacity
//!   changes (e.g., `BlockDeviceDisk` via Linux uevent, `NvmeDisk` via AEN)
//!   should override this. Decorators and layered disks delegate to the
//!   inner backend.
//!
//! # Error model
//!
//! All I/O methods return [`DiskError`], which frontends translate into
//! protocol-specific errors (NVMe status codes, SCSI sense keys). The
//! variants cover out-of-range LBAs, I/O errors, medium errors with
//! sub-classification, guest memory access failures, read-only violations,
//! persistent reservation conflicts, and unsupported eject.
//!
//! # Available backends
//!
//! | Backend | Crate | Description |
//! |---------|-------|-------------|
//! | `FileDisk` | `disk_file` | Host file, cross-platform |
//! | `Vhd1Disk` | `disk_vhd1` | VHD1 fixed format |
//! | `VhdmpDisk` | `disk_vhdmp` | Windows vhdmp driver |
//! | `BlobDisk` | `disk_blob` | Read-only HTTP / Azure Blob |
//! | `BlockDeviceDisk` | `disk_blockdevice` | Linux block device (io_uring) |
//! | `NvmeDisk` | `disk_nvme` | Physical NVMe (user-mode driver) |
//! | `StripedDisk` | `disk_striped` | Striped across multiple disks |
//! | `CryptDisk` | `disk_crypt` | XTS-AES-256 encryption wrapper |
//! | `DelayDisk` | `disk_delay` | Injected I/O latency wrapper |
//! | `DiskWithReservations` | `disk_prwrap` | In-memory PR emulation wrapper |
//! | `LayeredDisk` | `disk_layered` | Layered disk with per-sector presence |

#![forbid(unsafe_code)]

pub mod pr;
pub mod resolve;
pub mod sync_wrapper;

use guestmem::AccessError;
use inspect::Inspect;
use scsi_buffers::RequestBuffers;
use stackfuture::StackFuture;
use std::fmt::Debug;
use std::future::Future;
use std::future::ready;
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
    /// the backing store must also implement [`DiskIo::wait_resize`].
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

    /// Returns true if the `fua` parameter to [`DiskIo::write_vectored`] is
    /// respected by the backing store by ensuring that the IO is immediately
    /// committed to disk.
    fn is_fua_respected(&self) -> bool;

    /// Returns true if the disk is read only.
    fn is_read_only(&self) -> bool;

    /// Unmap sectors from the layer.
    fn unmap(
        &self,
        sector: u64,
        count: u64,
        block_level_only: bool,
    ) -> impl Future<Output = Result<(), DiskError>> + Send;

    /// Returns the behavior of the unmap operation.
    ///
    /// This tells callers what happens to the content of unmapped sectors:
    ///
    /// - [`UnmapBehavior::Zeroes`] — unmapped sectors read back as zero.
    /// - [`UnmapBehavior::Unspecified`] — content may or may not change, and
    ///   not necessarily to zero.
    /// - [`UnmapBehavior::Ignored`] — unmap is a no-op; content is unchanged.
    fn unmap_behavior(&self) -> UnmapBehavior;

    /// Returns the optimal granularity for unmaps, in sectors.
    fn optimal_unmap_sectors(&self) -> u32 {
        1
    }

    /// Optionally returns a trait object to issue persistent reservation
    /// requests.
    fn pr(&self) -> Option<&dyn pr::PersistentReservation> {
        None
    }

    /// Issues an asynchronous eject media operation to the disk.
    ///
    /// The default implementation returns [`DiskError::UnsupportedEject`].
    /// Eject is primarily a media state change managed by the SCSI DVD layer
    /// (`SimpleScsiDvd`), not by disk backends. Backends generally do not
    /// need to override this.
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

    /// Waits for the disk sector count to change from the specified value.
    ///
    /// Returns the new sector count once [`DiskIo::sector_count`] would return
    /// a value different from `sector_count`. Frontends use this to detect
    /// runtime capacity changes and notify the guest (NVMe via AEN, SCSI via
    /// UNIT_ATTENTION).
    ///
    /// The default implementation returns [`std::future::pending()`], meaning
    /// the disk never signals a resize. Only backends that can detect runtime
    /// capacity changes should override this — for example, `BlockDeviceDisk`
    /// (via Linux uevent) and `NvmeDisk` (via NVMe AEN). Decorator wrappers
    /// and `LayeredDisk` should delegate to the inner disk.
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
pub struct Disk(#[inspect(flatten)] Arc<DiskInner>);

impl Disk {
    fn inspect_extra(&self, resp: &mut inspect::Response<'_>) {
        resp.field("disk_type", self.0.disk.disk_type())
            .field("sector_count", self.0.disk.sector_count())
            .field("supports_pr", self.0.disk.pr().is_some());
    }
}

impl Debug for Disk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Disk").finish()
    }
}

#[derive(Inspect)]
#[inspect(bound = "T: DynDisk")]
struct DiskInner<T: ?Sized = dyn DynDisk> {
    sector_size: u32,
    sector_shift: u32,
    physical_sector_size: u32,
    disk_id: Option<[u8; 16]>,
    is_fua_respected: bool,
    is_read_only: bool,
    unmap_behavior: UnmapBehavior,
    optimal_unmap_sectors: u32,
    disk: T,
}

/// Errors that can occur when creating a `Disk`.
#[derive(Debug, Error)]
pub enum InvalidDisk {
    /// The sector size is invalid.
    #[error("invalid sector size: {0}")]
    InvalidSectorSize(u32),
    /// The physical sector size is invalid.
    #[error("invalid physical sector size: {0}")]
    InvalidPhysicalSectorSize(u32),
}

impl Disk {
    /// Returns a new disk wrapping the given backing object.
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
        Ok(Self(Arc::new(DiskInner {
            sector_size,
            sector_shift: sector_size.trailing_zeros(),
            physical_sector_size,
            disk_id: disk.disk_id(),
            is_fua_respected: disk.is_fua_respected(),
            is_read_only: disk.is_read_only(),
            optimal_unmap_sectors: disk.optimal_unmap_sectors(),
            unmap_behavior: disk.unmap_behavior(),
            disk,
        })))
    }

    /// Returns the current sector count.
    ///
    /// For some backing stores, this may change at runtime. Use
    /// [`wait_resize`](Self::wait_resize) to detect this change.
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

    /// Returns true if the `fua` parameter to
    /// [`write_vectored`](Self::write_vectored) is respected by the backing
    /// store by ensuring that the IO is immediately committed to disk.
    pub fn is_fua_respected(&self) -> bool {
        self.0.is_fua_respected
    }

    /// Returns true if the disk is read only.
    pub fn is_read_only(&self) -> bool {
        self.0.is_read_only
    }

    /// Unmap sectors from the disk.
    pub fn unmap(
        &self,
        sector: u64,
        count: u64,
        block_level_only: bool,
    ) -> impl use<'_> + Future<Output = Result<(), DiskError>> + Send {
        self.0.disk.unmap(sector, count, block_level_only)
    }

    /// Returns the behavior of the unmap operation.
    pub fn unmap_behavior(&self) -> UnmapBehavior {
        self.0.unmap_behavior
    }

    /// Returns the optimal granularity for unmaps, in sectors.
    pub fn optimal_unmap_sectors(&self) -> u32 {
        self.0.optimal_unmap_sectors
    }

    /// Optionally returns a trait object to issue persistent reservation
    /// requests.
    pub fn pr(&self) -> Option<&dyn pr::PersistentReservation> {
        self.0.disk.pr()
    }

    /// Issues an asynchronous eject media operation to the disk.
    pub fn eject(&self) -> impl use<'_> + Future<Output = Result<(), DiskError>> + Send {
        self.0.disk.eject()
    }

    /// Issues an asynchronous read-scatter operation to the disk.
    ///
    /// # Arguments
    ///
    /// * `buffers` - An object representing the data buffers into which the disk data will be transferred.
    /// * `sector` - The logical sector at which the read operation starts.
    pub fn read_vectored<'a>(
        &'a self,
        buffers: &'a RequestBuffers<'_>,
        sector: u64,
    ) -> impl use<'a> + Future<Output = Result<(), DiskError>> + Send {
        self.0.disk.read_vectored(buffers, sector)
    }

    /// Issues an asynchronous write-gather operation to the disk.
    ///
    /// # Arguments
    ///
    /// * `buffers` - An object representing the data buffers containing the data to transfer to the disk.
    /// * `sector` - The logical sector at which the write operation starts.
    /// * `fua` - A flag indicates if FUA (force unit access) is requested.
    ///
    /// # Panics
    ///
    /// The caller must pass a buffer with an integer number of sectors.
    pub fn write_vectored<'a>(
        &'a self,
        buffers: &'a RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> impl use<'a> + Future<Output = Result<(), DiskError>> + Send {
        self.0.disk.write_vectored(buffers, sector, fua)
    }

    /// Issues an asynchronous flush operation to the disk.
    pub fn sync_cache(&self) -> impl use<'_> + Future<Output = Result<(), DiskError>> + Send {
        self.0.disk.sync_cache()
    }

    /// Waits for the disk sector count to change from the specified value.
    pub fn wait_resize(&self, sector_count: u64) -> impl use<'_> + Future<Output = u64> {
        self.0.disk.wait_resize(sector_count)
    }
}

/// The behavior of the [`DiskIo::unmap`] operation.
///
/// This describes what happens to the content of unmapped sectors. Frontends
/// use this to report the correct behavior to the guest (e.g., SCSI
/// `LBPRZ` bit or NVMe DLFEAT field).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Inspect)]
pub enum UnmapBehavior {
    /// Unmap may or may not change the content, and not necessarily to zero.
    /// The guest cannot assume anything about the content of unmapped sectors.
    Unspecified,
    /// Unmaps are guaranteed to be ignored — the content is unchanged.
    /// The disk reports that unmap is not supported.
    Ignored,
    /// Unmap will deterministically zero the content. The guest can rely on
    /// reading back zeroes from unmapped sectors.
    Zeroes,
}

/// The amount of space reserved for a DiskIo future
///
/// This was chosen by running `cargo test -p storvsp -- --no-capture` and looking at the required
/// size that was given in the failure message
const ASYNC_DISK_STACK_SIZE: usize = 1256;

type IoFuture<'a> = StackFuture<'a, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }>;

trait DynDisk: Send + Sync + Inspect {
    fn disk_type(&self) -> &str;
    fn sector_count(&self) -> u64;

    fn unmap(&self, sector_offset: u64, sector_count: u64, block_level_only: bool) -> IoFuture<'_>;

    fn pr(&self) -> Option<&dyn pr::PersistentReservation>;
    fn eject(&self) -> IoFuture<'_>;

    fn read_vectored<'a>(&'a self, buffers: &'a RequestBuffers<'_>, sector: u64) -> IoFuture<'a>;

    fn write_vectored<'a>(
        &'a self,
        buffers: &'a RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> IoFuture<'a>;

    fn sync_cache(&self) -> IoFuture<'_>;

    fn wait_resize<'a>(
        &'a self,
        sector_count: u64,
    ) -> Pin<Box<dyn 'a + Send + Future<Output = u64>>> {
        let _ = sector_count;
        Box::pin(std::future::pending())
    }
}

impl<T: DiskIo> DynDisk for T {
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
        StackFuture::from_or_box(self.unmap(sector_offset, sector_count, block_level_only))
    }

    fn pr(&self) -> Option<&dyn pr::PersistentReservation> {
        self.pr()
    }

    fn eject(&self) -> IoFuture<'_> {
        StackFuture::from_or_box(self.eject())
    }

    fn read_vectored<'a>(&'a self, buffers: &'a RequestBuffers<'_>, sector: u64) -> IoFuture<'a> {
        StackFuture::from_or_box(self.read_vectored(buffers, sector))
    }

    fn write_vectored<'a>(
        &'a self,
        buffers: &'a RequestBuffers<'a>,
        sector: u64,
        fua: bool,
    ) -> IoFuture<'a> {
        StackFuture::from_or_box(self.write_vectored(buffers, sector, fua))
    }

    fn sync_cache(&self) -> IoFuture<'_> {
        StackFuture::from_or_box(self.sync_cache())
    }
}
