// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Qcow2 disk layer for OpenVMM.
//!
//! Provides a cross-platform, pure-Rust qcow2 backend for the layered disk
//! stack.
//!
//! # Modules
//!
//! - [`chain`] — qcow2 chain opening helpers
//! - [`resolver`] — resource resolver for qcow2 disk layers

#![forbid(unsafe_code)]

pub mod chain;
pub mod header;
pub mod resolver;

use disk_backend::DiskError;
use disk_backend::UnmapBehavior;
use disk_layered::LayerIo;
use disk_layered::SectorMarker;
use inspect::Inspect;
use scsi_buffers::RequestBuffers;

use crate::header::Qcow2Header;

/// A qcow2 disk layer implementing [`LayerIo`].
///
/// NOTE: This is a placeholder backing layer. Actual qcow2 format parsing
/// (header, L1/L2 tables, cluster allocation, backing file handling) is not
/// yet implemented.
#[derive(Inspect)]
pub struct Qcow2Layer {
    #[inspect(skip)]
    file: std::fs::File,
    sector_size: u32,
    sector_count: u64,
    read_only: bool,
}

impl Qcow2Layer {
    /// Create a `Qcow2Layer` from an open file.
    ///
    /// NOTE: `sector_count` is passed by the caller until format parsing is
    /// implemented.
    pub fn new(file: std::fs::File, header: Qcow2Header, read_only: bool) -> Self {
        let sector_count = header.size_bytes / 512;
        Self {
            file,
            sector_size: 512,
            sector_count,
            read_only,
        }
    }
}

impl LayerIo for Qcow2Layer {
    fn layer_type(&self) -> &str {
        "qcow2"
    }

    fn sector_count(&self) -> u64 {
        self.sector_count
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        None
    }

    fn physical_sector_size(&self) -> u32 {
        self.sector_size
    }

    fn is_fua_respected(&self) -> bool {
        false
    }

    fn is_logically_read_only(&self) -> bool {
        self.read_only
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        UnmapBehavior::Zeroes
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        self.file.sync_all().map_err(DiskError::Io)
    }

    async fn read(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        mut marker: SectorMarker<'_>,
    ) -> Result<(), DiskError> {
        use guestmem::MemoryWrite;
        let _ = buffers.writer().zero(0)?;
        let start_sector = sector;
        let sector_count = buffers.len() / self.sector_size as usize;
        marker.set_range(start_sector..start_sector + sector_count as u64);
        Ok(())
    }

    async fn write(
        &self,
        _buffers: &RequestBuffers<'_>,
        _sector: u64,
        _fua: bool,
    ) -> Result<(), DiskError> {
        Err(DiskError::Io(std::io::Error::other(
            "qcow2 writes are not implemented yet",
        )))
    }

    async fn unmap(
        &self,
        _sector: u64,
        _count: u64,
        _block_level_only: bool,
        _next_is_zero: bool,
    ) -> Result<(), DiskError> {
        Err(DiskError::Io(std::io::Error::other(
            "qcow2 unmap is not implemented yet",
        )))
    }
}
