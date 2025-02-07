// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A disk backend for "blobs", i.e. raw disk data that can be accessed through
//! a simple interface such as HTTP.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod blob;
pub mod resolver;

use blob::Blob;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::UnmapBehavior;
use guestmem::MemoryWrite;
use inspect::Inspect;
use scsi_buffers::RequestBuffers;
use std::sync::Arc;
use thiserror::Error;
use vhd1_defs::VhdFooter;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

const DEFAULT_SECTOR_SIZE: u32 = 512;

/// A read-only disk backed by a blob.
#[derive(Inspect)]
pub struct BlobDisk {
    blob: Arc<dyn Blob + Send + Sync>,
    sector_count: u64,
    sector_size: u32,
    sector_shift: u32,
    disk_id: Option<[u8; 16]>,
}

#[derive(Debug, Error)]
enum ErrorInner {
    #[error("blob is too small")]
    BlobTooSmall,
    #[error("failed to read the vhd footer")]
    VhdFooter(#[source] std::io::Error),
    #[error("invalid vhd1 footer cookie")]
    VhdFooterCookie,
    #[error("invalid vhd1 footer checksum")]
    VhdFooterChecksum,
    #[error("unsupported vhd version: {0:#x}")]
    UnsupportedVhdVersion(u32),
    #[error("not a fixed vhd")]
    NotFixedVhd,
    #[error("invalid disk size: {0}")]
    InvalidDiskSize(u64),
}

/// An error when attempting to open a blob in VHD1 format.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct Vhd1Error(#[from] ErrorInner);

impl BlobDisk {
    /// Returns a new blob disk where the blob is the raw disk data.
    pub fn new(blob: impl 'static + Blob + Send + Sync) -> Self {
        let blob = Arc::new(blob);
        let sector_count = blob.len() / DEFAULT_SECTOR_SIZE as u64;
        Self::new_inner(blob, sector_count, None)
    }

    /// Returns a new blob disk where the blob is a fixed VHD1.
    pub async fn new_fixed_vhd1(blob: impl 'static + Blob + Send + Sync) -> anyhow::Result<Self> {
        let blob = Arc::new(blob);
        let blob_len = blob.len();
        let footer_offset = blob_len
            .checked_sub(VhdFooter::LEN)
            .ok_or(ErrorInner::BlobTooSmall)?;

        let mut footer = VhdFooter::new_zeroed();
        blob.read(footer.as_mut_bytes(), footer_offset)
            .await
            .map_err(ErrorInner::VhdFooter)?;

        if footer.cookie != VhdFooter::COOKIE_MAGIC {
            return Err(ErrorInner::VhdFooterCookie.into());
        }
        if footer.checksum.get() != footer.compute_checksum() {
            return Err(ErrorInner::VhdFooterChecksum.into());
        }
        if footer.file_format_version.get() != VhdFooter::FILE_FORMAT_VERSION_MAGIC {
            return Err(ErrorInner::UnsupportedVhdVersion(footer.file_format_version.get()).into());
        }
        if footer.disk_type.get() != VhdFooter::DISK_TYPE_FIXED {
            return Err(ErrorInner::NotFixedVhd.into());
        }
        let disk_size = footer.current_size.get();
        if disk_size > footer_offset || disk_size % (DEFAULT_SECTOR_SIZE as u64) != 0 {
            return Err(ErrorInner::InvalidDiskSize(disk_size).into());
        }

        Ok(Self::new_inner(
            blob,
            disk_size / DEFAULT_SECTOR_SIZE as u64,
            Some(footer.unique_id.into()),
        ))
    }

    fn new_inner(
        blob: Arc<dyn Blob + Send + Sync>,
        sector_count: u64,
        disk_id: Option<[u8; 16]>,
    ) -> Self {
        Self {
            blob,
            sector_count,
            sector_size: DEFAULT_SECTOR_SIZE,
            sector_shift: DEFAULT_SECTOR_SIZE.trailing_zeros(),
            disk_id,
        }
    }
}

impl DiskIo for BlobDisk {
    fn disk_type(&self) -> &str {
        "blob"
    }

    fn sector_count(&self) -> u64 {
        self.sector_count
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        self.disk_id
    }

    fn physical_sector_size(&self) -> u32 {
        4096
    }

    fn is_fua_respected(&self) -> bool {
        false
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        let mut buf = vec![0; buffers.len()];
        self.blob
            .read(&mut buf, sector << self.sector_shift)
            .await
            .map_err(DiskError::Io)?;

        buffers.writer().write(&buf)?;
        Ok(())
    }

    async fn write_vectored(
        &self,
        _buffers: &RequestBuffers<'_>,
        _sector: u64,
        _fua: bool,
    ) -> Result<(), DiskError> {
        Err(DiskError::ReadOnly)
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        Err(DiskError::ReadOnly)
    }

    async fn unmap(
        &self,
        _sector: u64,
        _count: u64,
        _block_level_only: bool,
    ) -> Result<(), DiskError> {
        Err(DiskError::ReadOnly)
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        UnmapBehavior::Ignored
    }
}
