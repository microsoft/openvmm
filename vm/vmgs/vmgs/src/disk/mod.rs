// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Abstraction over block storage devices.
//!
//! The [`BlockStorage`] trait is public, and can be implemented outside of the
//! `vmgs` crate. e.g: VMGS over GET is implemented in another crate.

use async_trait::async_trait;
use std::io;

pub mod vhd_file;

/// Fixed metadata associated with a particular [`BlockStorage`] instance.
#[allow(missing_docs)] // self explanatory field names
#[derive(Copy, Clone, Debug)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
pub struct BlockStorageMetadata {
    pub capacity: u64,
    pub logical_sector_size: u32,
    pub sector_count: u64,
    pub sector_size: u32,
    pub physical_sector_size: u32,
    pub max_trans_size_bytes: u32,
}

/// Abstraction over a block storage device.
#[async_trait]
pub trait BlockStorage: Send + Sync {
    /// Read a block from the block device.
    async fn read_block(&mut self, byte_offset: u64, buf: &mut [u8]) -> io::Result<()>;
    /// Write a block to the block device.
    async fn write_block(&mut self, byte_offset: u64, buf: &[u8]) -> io::Result<()>;
    /// Flush any buffered data.
    async fn flush(&mut self) -> io::Result<()>;

    /// Return fixed metadata associated with a particualar [`BlockStorage`] instance.
    fn meta(&self) -> BlockStorageMetadata;

    /// Validate the maximum transfer size is correct. No-op by default,
    /// validation is only for Underhill, for the GET/vmbus transfer size
    fn validate_transfer_size(&self, _max_trans_size_bytes: u32) -> io::Result<()> {
        Ok(())
    }
}

macro_rules! impl_dyn {
    ($ty:ty) => {
        #[async_trait]
        impl BlockStorage for $ty {
            async fn read_block(&mut self, byte_offset: u64, buf: &mut [u8]) -> io::Result<()> {
                (**self).read_block(byte_offset, buf).await
            }

            async fn write_block(&mut self, byte_offset: u64, buf: &[u8]) -> io::Result<()> {
                (**self).write_block(byte_offset, buf).await
            }

            async fn flush(&mut self) -> io::Result<()> {
                (**self).flush().await
            }

            fn meta(&self) -> BlockStorageMetadata {
                (**self).meta()
            }

            fn validate_transfer_size(&self, max_trans_size_bytes: u32) -> io::Result<()> {
                (**self).validate_transfer_size(max_trans_size_bytes)
            }
        }
    };
}

impl_dyn!(Box<dyn BlockStorage>);
impl_dyn!(Box<dyn BlockStorage + Send>);
impl_dyn!(Box<dyn BlockStorage + Send + Sync>);
impl_dyn!(&mut dyn BlockStorage);
