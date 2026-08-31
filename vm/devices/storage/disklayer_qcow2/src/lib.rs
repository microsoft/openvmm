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
//! - [`header`] — qcow2 on-disk header parsing
//! - [`resolver`] — resource resolver for qcow2 disk layers
//! - [`table`] — qcow2 L1/L2 table entry parsing

#![forbid(unsafe_code)]

pub mod chain;
mod header;
pub mod resolver;
mod table;

use disk_backend::DiskError;
use disk_backend::UnmapBehavior;
use disk_layered::LayerIo;
use disk_layered::SectorMarker;
use inspect::Inspect;
use scsi_buffers::RequestBuffers;
use std::io::Seek;

use crate::header::Qcow2Header;
use crate::table::L1Entry;
use crate::table::read_l1_table;

const SECTOR_SIZE: u32 = 512;

/// A qcow2 disk layer implementing [`LayerIo`].
///
/// NOTE: This is a placeholder backing layer. Actual qcow2 format parsing
/// (header, L1/L2 tables, cluster allocation, backing file handling) is not
/// yet implemented.
#[derive(Inspect)]
pub struct Qcow2Layer {
    #[inspect(skip)]
    file: std::fs::File,
    header: Qcow2Header,
    sector_count: u64,
    read_only: bool,
    #[inspect(skip)]
    l1_table: Vec<L1Entry>,
}

impl Qcow2Layer {
    /// Create a `Qcow2Layer` from an open file.
    ///
    /// NOTE: `sector_count` is passed by the caller until format parsing is
    /// implemented.
    pub fn new(mut file: std::fs::File, header: Qcow2Header, read_only: bool) -> anyhow::Result<Self> {
        let sector_count = header.size_bytes / SECTOR_SIZE as u64;

        use std::io::Read;
        file.seek(std::io::SeekFrom::Start(header.l1_table_offset))?;
        let mut l1_bytes = vec![0u8; header.l1_size as usize * 8];
        file.read_exact(&mut l1_bytes)?;
        let mut l1_slice = l1_bytes.as_slice();
        let l1_table = read_l1_table(&mut l1_slice, header.l1_size)?;

        Ok(Self {
            file,
            header,
            sector_count,
            read_only,
            l1_table,
        })
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
        SECTOR_SIZE
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        None
    }

    fn physical_sector_size(&self) -> u32 {
        SECTOR_SIZE
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
        let _ = (buffers, sector, marker);
        Err(DiskError::Io(std::io::Error::other(
            "qcow2 reads are not implemented yet",
        )))
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
