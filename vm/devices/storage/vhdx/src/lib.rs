// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Pure-Rust VHDX file format support.
//!
//! This crate is being built bottom-up. The initial surface contains the
//! on-disk format definitions, checksum helpers, and error taxonomy used by
//! later parsing and I/O layers.

#![forbid(unsafe_code)]
#![allow(async_fn_in_trait)]

use std::borrow::Borrow;
use std::future::Future;

pub(crate) mod apply_task;
pub(crate) mod cache;
pub mod error;
pub(crate) mod flush;
pub mod format;
pub mod log;
pub(crate) mod log_permits;
pub(crate) mod log_task;
pub(crate) mod lsn_watermark;

pub use error::CreateError;
pub use error::InvalidFormatReason;
pub use error::OpenError;
pub use error::VhdxIoError;
pub use error::VhdxIoErrorKind;

#[cfg(test)]
mod tests;

/// Trait abstracting metadata file I/O for the VHDX parser.
///
/// The crate uses this trait for internal metadata access. Payload data I/O is
/// added in later chunks, once the parser can translate virtual disk ranges to
/// file offsets.
pub trait AsyncFile: Send + Sync {
    /// Buffer type for owned I/O operations.
    type Buffer: AsRef<[u8]> + AsMut<[u8]> + Clone + Send + Sync + 'static;

    /// Allocate a zero-initialized buffer of the given length.
    fn alloc_buffer(&self, len: usize) -> Self::Buffer;

    /// Read from the file into an owned buffer. Returns the filled buffer.
    fn read_into(
        &self,
        offset: u64,
        buf: Self::Buffer,
    ) -> impl Future<Output = Result<Self::Buffer, std::io::Error>> + Send;

    /// Write a buffer to the file at the given offset.
    fn write_from(
        &self,
        offset: u64,
        buf: impl Borrow<Self::Buffer> + Send + 'static,
    ) -> impl Future<Output = Result<(), std::io::Error>> + Send;

    /// Flush all buffered writes to stable storage.
    fn flush(&self) -> impl Future<Output = Result<(), std::io::Error>> + Send;

    /// Return the current size of the file in bytes.
    fn file_size(&self) -> impl Future<Output = Result<u64, std::io::Error>> + Send;

    /// Set the file to the given size in bytes.
    fn set_file_size(&self, size: u64) -> impl Future<Output = Result<(), std::io::Error>> + Send;

    /// Zero a byte range of the file.
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
/// methods for tests.
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
