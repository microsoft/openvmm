// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SQLite-backed disk layer implementation.
//!
//! At this time, **this layer is only designed for use in dev/test scenarios!**
//!
//! # DISCLAIMER: Stability
//!
//! There are no stability guarantees around the on-disk data format! The schema
//! can and will change without warning!
//!
//! # DISCLAIMER: Performance
//!
//! This implementation has only been minimally optimized! Don't expect to get
//! incredible perf from this disk backend!
//!
//! Notably:
//!
//! - Data is stored within a single `sectors` table as tuples of `(sector:
//!   INTEGER, sector_data: BLOB(sector_size))`. All data is accessed in
//!   `sector_size` chunks (i.e: without performing any kind of adjacent-sector
//!   coalescing).
//! - Reads and writes currently allocate many temporary `Vec<u8>` buffers per
//!   operation, without any buffer reuse.
//!
//! These design choices were made with simplicity and expediency in mind, given
//! that the primary use-case for this backend is for dev/test scenarios. If
//! performance ever becomes a concern, there are various optimizations that
//! should be possible to implement here, though quite frankly, investing in a
//! cross-platform QCOW2 or VHDX disk backend is likely a far more worthwhile
//! endeavor.
//!
//! # Context
//!
//! In late 2024, OpenVMM was missing a _cross-platform_ disk backend that
//! supported the following key features:
//!
//! - Used a dynamically-sized file as the disks's backing store
//! - Supported snapshots / differencing disks
//!
//! While OpenVMM will eventually need to support for one or more of the current
//! "industry standard" virtual disk formats that supports these features (e.g:
//! QCOW2, VHDX), we really wanted some sort of "stop-gap" solution to unblock
//! various dev/test use-cases.
//!
//! And thus, `disklayer_sqlite` was born!
//!
//! The initial implementation took less than a day to get up and running, and
//! worked "well enough" to support the dev/test scenarios we were interested
//! in, such as:
//!
//! - Having a cross-platform _sparsely allocated_ virtual disk file.
//! - Having a _persistent_ diff-disk on-top of an existing disk (as opposed to
//!   `ramdiff`, which is in-memory and _ephemeral_)
//! - Having a "cache" layer for JIT-accessed disks, such as `disk_blob`
//!
//! The idea of using SQLite as a backing store - while wacky - proved to be an
//! excellent way to quickly bring up a dynamically-sized, sparsely-allocated
//! disk format for testing in OpenVMM.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod resolver;

use anyhow::Context;
use disk_backend::DiskError;
use disk_backend::UnmapBehavior;
use disk_layered::LayerAttach;
use disk_layered::LayerIo;
use disk_layered::SectorMarker;
use disk_layered::WriteNoOverwrite;
use inspect::Inspect;
use scsi_buffers::RequestBuffers;

/// Formatting parameters provided to [`FormatOnAttachSqliteDiskLayer::new`].
///
/// Optional parameters which are not provided will be determined by reading the
/// metadata of the layer being attached to.
#[derive(Inspect, Copy, Clone)]
pub struct IncompleteFormatParams {
    /// Should the layer be considered logically read only (i.e: a cache layer)
    pub logically_read_only: bool,
    /// The size of the layer in bytes.
    pub len: Option<u64>,
}

/// Formatting parameters provided to [`SqliteDiskLayer::new`]
#[derive(Inspect, Copy, Clone)]
pub struct FormatParams {
    /// Should the layer be considered logically read only (i.e: a cache layer)
    pub logically_read_only: bool,
    /// The size of the layer in bytes.
    pub len: u64,
}

/// A disk layer backed by sqlite, which lazily infers its topology from the
/// layer it is being stacked on-top of.
#[derive(Inspect)]
#[non_exhaustive]
pub struct FormatOnAttachSqliteDiskLayer {
    dbhd_path: String,
    format_dbhd: IncompleteFormatParams,
}

impl FormatOnAttachSqliteDiskLayer {
    /// Create a new sqlite-backed disk layer, which is formatted when it is
    /// attached.
    pub fn new(dbhd_path: String, format_dbhd: IncompleteFormatParams) -> anyhow::Result<Self> {
        Ok(Self {
            dbhd_path,
            format_dbhd,
        })
    }
}

/// A disk layer backed entirely by sqlite.
#[derive(Inspect)]
pub struct SqliteDiskLayer {
    // TODO
}

impl SqliteDiskLayer {
    /// Create a new sqlite-backed disk layer.
    pub fn new(dbhd_path: String, format_dbhd: Option<FormatParams>) -> anyhow::Result<Self> {
        todo!()
    }

    fn write_maybe_overwrite(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        overwrite: bool,
    ) -> Result<(), DiskError> {
        todo!()
    }
}

impl LayerAttach for FormatOnAttachSqliteDiskLayer {
    type Error = anyhow::Error;
    type Layer = SqliteDiskLayer;

    async fn attach(
        self,
        lower_layer_metadata: Option<disk_layered::DiskLayerMetadata>,
    ) -> Result<Self::Layer, Self::Error> {
        let len = {
            let lower_len = lower_layer_metadata.map(|m| m.sector_count * m.sector_size as u64);
            self.format_dbhd
                .len
                .or(lower_len)
                .context("no base layer to infer sector_count from")?
        };

        SqliteDiskLayer::new(
            self.dbhd_path,
            Some(FormatParams {
                logically_read_only: self.format_dbhd.logically_read_only,
                len,
            }),
        )
    }
}

impl LayerIo for SqliteDiskLayer {
    fn layer_type(&self) -> &str {
        "sqlite"
    }

    fn sector_count(&self) -> u64 {
        todo!()
    }

    fn sector_size(&self) -> u32 {
        todo!()
    }

    fn is_read_only(&self) -> bool {
        todo!()
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        todo!()
    }

    fn physical_sector_size(&self) -> u32 {
        todo!()
    }

    fn is_fua_respected(&self) -> bool {
        todo!()
    }

    async fn read(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        mut marker: SectorMarker<'_>,
    ) -> Result<(), DiskError> {
        todo!()
    }

    async fn write(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        _fua: bool,
    ) -> Result<(), DiskError> {
        self.write_maybe_overwrite(buffers, sector, true)
    }

    fn write_no_overwrite(&self) -> Option<impl WriteNoOverwrite> {
        Some(self)
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        todo!()
    }

    async fn unmap(
        &self,
        sector_offset: u64,
        sector_count: u64,
        _block_level_only: bool,
        next_is_zero: bool,
    ) -> Result<(), DiskError> {
        todo!()
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        // This layer zeroes if the lower layer is zero, but otherwise does
        // nothing, so we must report unspecified.
        UnmapBehavior::Unspecified
    }

    fn optimal_unmap_sectors(&self) -> u32 {
        1
    }
}

impl WriteNoOverwrite for SqliteDiskLayer {
    async fn write_no_overwrite(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        self.write_maybe_overwrite(buffers, sector, false)
    }
}
