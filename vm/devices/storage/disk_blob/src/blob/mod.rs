// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The blob trait and blob implementations.

pub mod file;
pub mod http;

use async_trait::async_trait;
use inspect::Inspect;

/// Trait for a read-only blob.
#[async_trait]
pub trait Blob: Inspect {
    /// Reads data at `offset` into `buf`.
    ///
    /// If `buf` is not completely filled, then this should return
    /// [`std::io::ErrorKind::UnexpectedEof`] rather than any kind of partial
    /// success.
    async fn read(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()>;

    /// Returns the length of the blob in bytes.
    fn len(&self) -> u64;
}
