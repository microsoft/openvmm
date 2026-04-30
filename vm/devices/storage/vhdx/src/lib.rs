// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Pure-Rust VHDX file format parser and writer.
//!
//! This crate implements the
//! [VHDX format specification](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-vhdx/)
//! with no platform-specific dependencies, enabling cross-platform support
//! for dynamic, fixed, and differencing VHDX virtual hard disk files.
//!
//! # Overview
//!
//! A VHDX file stores a virtual disk as a collection of fixed-size data
//! blocks tracked by a Block Allocation Table (BAT). Crash consistency is
//! maintained through a write-ahead log (WAL) that journals metadata
//! changes before they reach their final file locations.
//!
//! ## Lifecycle
//!
//! ```text
//! create()  ──►  VhdxFile::open(file)  ──►  VhdxBuilder
//!                                             ├── .read_only()  ──►  VhdxFile (read)
//!                                             └── .writable()   ──►  VhdxFile (read/write)
//!                                                                     │
//!                                                   resolve_read / resolve_write / flush / trim
//!                                                                     │
//!                                                                   close()
//! ```
//!
//! 1. **Create** — [`create::create()`] writes a valid, empty VHDX file.
//! 2. **Open** — [`VhdxFile::open()`] returns a [`VhdxBuilder`] for
//!    configuring options (block alignment, log replay policy) before
//!    calling [`read_only()`](VhdxBuilder::read_only) or
//!    [`writable()`](VhdxBuilder::writable).
//! 3. **I/O** — [`VhdxFile::resolve_read()`](open::VhdxFile::resolve_read)
//!    and [`VhdxFile::resolve_write()`](open::VhdxFile::resolve_write)
//!    translate virtual disk offsets into file-level ranges. The caller
//!    performs actual data I/O at the returned offsets, then calls
//!    [`WriteIoGuard::complete()`] to finalize metadata.
//! 4. **Flush** — [`VhdxFile::flush()`](open::VhdxFile::flush) commits
//!    dirty pages through the WAL and flushes to stable storage.
//! 5. **Close** — [`VhdxFile::close()`](open::VhdxFile::close) drains the
//!    pipeline and clears the log GUID, leaving the file clean.
//!
//! ## Write pipeline (cache → log → apply)
//!
//! Writable opens spawn two background tasks that form a three-stage
//! pipeline for crash-consistent metadata persistence:
//!
//! ```text
//! ┌───────────┐    commit()    ┌──────────┐    apply    ┌────────────┐
//! │   Cache   │ ──────────────►│ Log Task │ ───────────►│ Apply Task │
//! │ (dirty    │   dirty pages  │ (WAL     │  logged     │ (final     │
//! │  pages)   │                │  writer) │  pages      │  offsets)  │
//! └───────────┘                └──────────┘             └────────────┘
//!       ▲                           │                         │
//!   LogPermits                  logged_lsn              applied_lsn
//!  (backpressure)              (LsnWatermark)           (LsnWatermark)
//! ```
//!
//! - The **cache** accumulates dirty 4 KiB metadata pages (BAT entries,
//!   sector bitmap bits). On commit, pages are sent to the log task.
//! - The **log task** writes WAL entries to the circular log region and
//!   publishes `logged_lsn`.
//! - The **apply task** writes logged pages to their final file offsets
//!   and publishes `applied_lsn`.
//! - A permit semaphore limits in-flight pages for backpressure. A flush
//!   sequencer coalesces concurrent flush requests.
//!
//! # I/O model
//!
//! The crate separates **metadata I/O** from **payload I/O**.
//!
//! Metadata I/O (headers, BAT pages, sector bitmaps, WAL entries) is
//! handled internally through [`AsyncFile`] — the caller provides an
//! implementation at open time and never thinks about metadata again.
//!
//! Payload I/O (guest data reads and writes) is the caller's
//! responsibility. [`resolve_read()`](open::VhdxFile::resolve_read) and
//! [`resolve_write()`](open::VhdxFile::resolve_write) translate virtual
//! disk offsets into file-level byte ranges ([`ReadRange`] /
//! [`WriteRange`]). The caller performs its own data I/O at those
//! offsets using whatever mechanism it prefers (io_uring, standard file
//! I/O, etc.), then finalizes metadata via the returned I/O guard.
//! This separation lets the caller use a different, potentially more
//! performant I/O path for bulk data without the crate imposing any
//! particular strategy.

#![forbid(unsafe_code)]
#![allow(async_fn_in_trait)]

use std::borrow::Borrow;
use std::future::Future;

pub(crate) mod apply_task;
pub(crate) mod bat;
pub(crate) mod cache;
pub(crate) mod create;
pub(crate) mod error;
pub(crate) mod flush;
pub(crate) mod format;
pub(crate) mod header;
pub(crate) mod io;
pub(crate) mod known_meta;
pub(crate) mod locator;
pub(crate) mod log;
pub(crate) mod log_permits;
pub(crate) mod log_task;
pub(crate) mod lsn_watermark;
pub(crate) mod metadata;
pub(crate) mod open;
pub(crate) mod region;
pub(crate) mod sector_bitmap;
pub(crate) mod space;
pub(crate) mod trim;

pub use create::CreateParams;
pub use create::create;
pub use error::CreateError;
pub use error::InvalidFormatReason;
pub use error::OpenError;
pub use error::VhdxIoError;
pub use error::VhdxIoErrorKind;
pub use io::ReadIoGuard;
pub use io::ReadRange;
pub use io::WriteIoGuard;
pub use io::WriteRange;
pub use locator::LocatorKeyValue;
pub use locator::ParentLocator;
pub use locator::ParentPaths;
pub use open::VhdxBuilder;
pub use open::VhdxFile;
pub use trim::TrimMode;
pub use trim::TrimRequest;

#[cfg(test)]
mod tests;

/// Trait abstracting metadata file I/O for the VHDX parser.
///
/// The crate uses this trait for all internal metadata access (headers,
/// BAT pages, sector bitmaps, WAL entries, log replay). Payload data
/// I/O is **not** routed through this trait — the caller handles it
/// directly at the file offsets returned by
/// [`VhdxFile::resolve_read()`](open::VhdxFile::resolve_read) and
/// [`VhdxFile::resolve_write()`](open::VhdxFile::resolve_write).
///
/// All async methods return `Send` futures so that the log task (spawned
/// on a multi-threaded executor) can call them.
///
/// This trait is **not** dyn-compatible due to `impl Future` return types.
/// When dynamic dispatch is needed (e.g. `disk_backend` integration),
/// create a separate dyn-compatible wrapper trait with a blanket impl.
pub trait AsyncFile: Send + Sync {
    /// Buffer type for owned I/O operations.
    ///
    /// Implementations control allocation strategy (e.g., alignment for
    /// O_DIRECT). Buffers are owned and `'static`, so they can be safely
    /// moved into `blocking::unblock`, io_uring submissions, etc.
    type Buffer: AsRef<[u8]> + AsMut<[u8]> + Clone + Send + Sync + 'static;

    /// Allocate a zero-initialized buffer of the given length.
    fn alloc_buffer(&self, len: usize) -> Self::Buffer;

    /// Read from the file into an owned buffer. Returns the filled buffer.
    ///
    /// The read starts at `offset` and fills `buf.as_mut().len()` bytes.
    fn read_into(
        &self,
        offset: u64,
        buf: Self::Buffer,
    ) -> impl Future<Output = Result<Self::Buffer, std::io::Error>> + Send;

    /// Write a buffer to the file at the given offset.
    ///
    /// Accepts any type that borrows as `Self::Buffer`, enabling zero-copy
    /// writes from `Arc<Self::Buffer>` (the `Arc` moves into the I/O
    /// closure; data is borrowed in place without copying).
    fn write_from(
        &self,
        offset: u64,
        buf: impl Borrow<Self::Buffer> + Send + 'static,
    ) -> impl Future<Output = Result<(), std::io::Error>> + Send;

    /// Flush all buffered writes to stable storage.
    fn flush(&self) -> impl Future<Output = Result<(), std::io::Error>> + Send;

    /// Return the current size of the file in bytes.
    fn file_size(&self) -> impl Future<Output = Result<u64, std::io::Error>> + Send;

    /// Set (truncate or extend) the file to the given size in bytes.
    fn set_file_size(&self, size: u64) -> impl Future<Output = Result<(), std::io::Error>> + Send;

    /// Zero a byte range of the file.
    ///
    /// Implementations may use platform-specific APIs (e.g., `fallocate`
    /// with `FALLOC_FL_ZERO_RANGE` on Linux, or `FSCTL_SET_ZERO_DATA` on
    /// Windows) for efficiency. The default implementation writes zeros
    /// in fixed-size chunks via [`write_from`](Self::write_from).
    fn zero_range(
        &self,
        offset: u64,
        len: u64,
    ) -> impl Future<Output = Result<(), std::io::Error>> + Send {
        async move {
            const CHUNK: usize = 64 * 1024;
            let zeros = self.alloc_buffer(CHUNK);
            let mut remaining = len;
            let mut pos = offset;
            while remaining > 0 {
                let n = (remaining as usize).min(CHUNK);
                if n < CHUNK {
                    let small = self.alloc_buffer(n);
                    self.write_from(pos, small).await?;
                } else {
                    self.write_from(pos, zeros.clone()).await?;
                }
                pos += n as u64;
                remaining -= n as u64;
            }
            Ok(())
        }
    }
}

/// Extension trait providing slice-based `read_at`/`write_at` convenience
/// methods. These allocate a temporary buffer internally, so they involve
/// an extra copy compared to `read_into`/`write_from`.
///
/// Automatically implemented for all [`AsyncFile`] types.
///
/// Only used for tests within this crate, not to be exposed publicly.
#[cfg(test)]
pub trait AsyncFileExt: AsyncFile {
    /// Read exactly `buf.len()` bytes from the file at the given byte offset.
    fn read_at(
        &self,
        offset: u64,
        buf: &mut [u8],
    ) -> impl Future<Output = Result<(), std::io::Error>> + Send {
        async move {
            let owned = self.alloc_buffer(buf.len());
            let owned = self.read_into(offset, owned).await?;
            buf.copy_from_slice(owned.as_ref());
            Ok(())
        }
    }

    /// Write exactly `buf.len()` bytes to the file at the given byte offset.
    fn write_at(
        &self,
        offset: u64,
        buf: &[u8],
    ) -> impl Future<Output = Result<(), std::io::Error>> + Send {
        async move {
            let mut owned = self.alloc_buffer(buf.len());
            owned.as_mut().copy_from_slice(buf);
            self.write_from(offset, owned).await
        }
    }
}

#[cfg(test)]
impl<T: AsyncFile> AsyncFileExt for T {}
