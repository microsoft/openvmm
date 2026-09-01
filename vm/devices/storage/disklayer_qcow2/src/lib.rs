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
mod readwriteat;
mod refcount;
pub mod resolver;
mod table;
#[cfg(test)]
mod tests;

use blocking::unblock;
use core::cmp::min;
use disk_backend::DiskError;
use disk_backend::UnmapBehavior;
use disk_layered::LayerIo;
use disk_layered::SectorMarker;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use inspect::Inspect;
use scsi_buffers::RequestBuffers;
use std::fs::File;
use std::sync::Arc;

use crate::header::Qcow2Header;
use crate::readwriteat::ReadWriteAt;
use crate::refcount::RefcountTable;
use crate::refcount::allocate_cluster;
use crate::refcount::zero_cluster;
use crate::table::ClusterAddress;
use crate::table::L1Entry;
use crate::table::L2Entry;
use crate::table::read_l1_table;
use crate::table::read_l2_table;
use crate::table::split_guest_offset;
use crate::table::write_l1_entry;
use crate::table::write_l2_table;
use futures::lock::Mutex;

const SECTOR_SIZE: u32 = 512;

/// Mutable state shared across all I/O tasks for a qcow2 image.
struct LayerState {
    l1_table: Vec<L1Entry>,
    refcounts: RefcountTable,
}

/// A qcow2 disk layer implementing [`LayerIo`].
///
/// NOTE: This is a placeholder backing layer. Actual qcow2 format parsing
/// (header, L1/L2 tables, cluster allocation, backing file handling) is not
/// yet implemented.
#[derive(Inspect)]
pub struct Qcow2Layer {
    #[inspect(skip)]
    file: Arc<File>,
    header: Qcow2Header,
    sector_count: u64,
    read_only: bool,
    #[inspect(skip)]
    state: Mutex<LayerState>,
}

impl Qcow2Layer {
    /// Create a `Qcow2Layer` from an open file.
    ///
    /// NOTE: `sector_count` is passed by the caller until format parsing is
    /// implemented.
    pub fn new(file: File, header: Qcow2Header, read_only: bool) -> anyhow::Result<Self> {
        let sector_count = header.size_bytes / SECTOR_SIZE as u64;

        let mut l1_bytes = vec![0u8; header.l1_size as usize * 8];
        file.read_at(&mut l1_bytes, header.l1_table_offset)?;
        let mut l1_slice = l1_bytes.as_slice();
        let l1_table = read_l1_table(&mut l1_slice, header.l1_size)?;

        let mut refcounts = RefcountTable::new(&header)?;
        if header.refcount_table_offset != 0 && header.refcount_table_clusters != 0 {
            let table_bytes_len = header.refcount_table_clusters as usize * header.cluster_size() as usize;
            let mut table_bytes = vec![0u8; table_bytes_len];
            file.read_at(&mut table_bytes, header.refcount_table_offset)?;
            refcounts.set_table_bytes(&table_bytes);
        }

        Ok(Self {
            file: Arc::new(file),
            header,
            sector_count,
            read_only,
            state: Mutex::new(LayerState {
                l1_table,
                refcounts,
            }),
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
        let file = self.file.clone();
        unblock(move || file.sync_all())
            .await
            .map_err(DiskError::Io)
    }

    async fn read(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        mut marker: SectorMarker<'_>,
    ) -> Result<(), DiskError> {
        let offset = sector * SECTOR_SIZE as u64;
        let len = buffers.len();
        let cluster_size = self.header.cluster_size() as usize;
        let l2_entries = self.header.l2_entries_per_table() as usize;

        let file = self.file.clone();
        let state = self.state.lock().await;
        let l1_table = &state.l1_table;

        let mut byte_off = offset;
        let end = offset + len as u64;
        while byte_off < end {
            let addr: ClusterAddress = split_guest_offset(&self.header, byte_off);
            if addr.l1_index as usize >= l1_table.len() {
                return Err(DiskError::IllegalBlock);
            }

            let l1_entry = &l1_table[addr.l1_index as usize];
            if l1_entry.l2_offset == 0 {
                // Unallocated; zero the covered portion of the request.
                let zero_n = min(
                    (end - byte_off) as usize,
                    cluster_size - addr.in_cluster_offset as usize,
                );
                let buf_off = (byte_off - offset) as usize;
                buffers.subrange(buf_off, zero_n).writer().zero(zero_n)?;
                let start_sector = byte_off / SECTOR_SIZE as u64;
                let sector_count = zero_n as u64 / SECTOR_SIZE as u64;
                marker.set_range(start_sector..start_sector + sector_count);
                byte_off += zero_n as u64;
                continue;
            }

            // TODO: Add L2 table caching for performance.
            let l2_table_offset = l1_entry.l2_offset;
            let mut l2_bytes = vec![0u8; l2_entries * 8];
            let f = file.clone();
            let l2_bytes = unblock(move || -> Result<Vec<u8>, std::io::Error> {
                f.read_at(&mut l2_bytes, l2_table_offset)?;
                Ok(l2_bytes)
            })
            .await
            .map_err(DiskError::Io)?;
            let mut l2_slice = l2_bytes.as_slice();
            let l2_table = read_l2_table(&mut l2_slice, l2_entries as u32)
                .map_err(|e| DiskError::Io(std::io::Error::other(e)))?;
            let l2_entry: &L2Entry = &l2_table[addr.l2_index as usize];

            if l2_entry.cluster_offset == 0 {
                // Unallocated; zero the covered portion of the request.
                let zero_n = min(
                    (end - byte_off) as usize,
                    cluster_size - addr.in_cluster_offset as usize,
                );
                let buf_off = (byte_off - offset) as usize;
                buffers.subrange(buf_off, zero_n).writer().zero(zero_n)?;
                let start_sector = byte_off / SECTOR_SIZE as u64;
                let sector_count = zero_n as u64 / SECTOR_SIZE as u64;
                marker.set_range(start_sector..start_sector + sector_count);
                byte_off += zero_n as u64;
                continue;
            }
            if l2_entry.compressed {
                return Err(DiskError::InvalidInput);
            }

            let bytes_in_cluster = min(
                (end - byte_off) as usize,
                cluster_size - addr.in_cluster_offset as usize,
            );
            let file_offset = l2_entry.cluster_offset + addr.in_cluster_offset;

            let mut data = vec![0u8; bytes_in_cluster];
            let f = file.clone();
            let data = unblock(move || -> Result<Vec<u8>, std::io::Error> {
                f.read_at(&mut data, file_offset)?;
                Ok(data)
            })
            .await
            .map_err(DiskError::Io)?;

            let buf_off = (byte_off - offset) as usize;
            buffers
                .subrange(buf_off, bytes_in_cluster)
                .writer()
                .write(&data)?;

            let start_sector = byte_off / SECTOR_SIZE as u64;
            let sector_count = bytes_in_cluster as u64 / SECTOR_SIZE as u64;
            marker.set_range(start_sector..start_sector + sector_count);

            byte_off += bytes_in_cluster as u64;
        }

        Ok(())
    }

    async fn write(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        _fua: bool,
    ) -> Result<(), DiskError> {
        let offset = sector * SECTOR_SIZE as u64;
        let len = buffers.len();
        let cluster_size = self.header.cluster_size() as usize;
        let l2_entries = self.header.l2_entries_per_table() as usize;
        let file = self.file.clone();
        let mut state = self.state.lock().await;
        let mut byte_off = offset;
        let end = offset + len as u64;

        while byte_off < end {
            let addr: ClusterAddress = split_guest_offset(&self.header, byte_off);
            if addr.l1_index as usize >= state.l1_table.len() {
                return Err(DiskError::IllegalBlock);
            }

            let bytes_in_cluster = min(
                (end - byte_off) as usize,
                cluster_size - addr.in_cluster_offset as usize,
            );

            // TODO: once refcounts exist, this should COW if the refcount is
            // shared with a backing file / snapshot.
            let l2_offset = state.l1_table[addr.l1_index as usize].l2_offset;
            let l2_offset = if l2_offset == 0 {
                let cluster_size = cluster_size as u64;
                let new_l2 = allocate_cluster(file.clone(), cluster_size).await?;
                zero_cluster(file.clone(), new_l2, cluster_size as usize).await?;
                state
                    .refcounts
                    .increment_cluster(&file, new_l2 / cluster_size)
                    .await?;

                state.l1_table[addr.l1_index as usize].l2_offset = new_l2;
                let f = file.clone();
                let l1_table_offset = self.header.l1_table_offset;
                unblock(move || write_l1_entry(&f, l1_table_offset, addr.l1_index, new_l2))
                    .await
                    .map_err(DiskError::Io)?;

                new_l2
            } else {
                l2_offset
            };

            // TODO: Read cache for Level 2 entries
            let mut l2_bytes = vec![0u8; l2_entries * 8];
            let f = file.clone();
            let l2_bytes = unblock(move || -> Result<Vec<u8>, std::io::Error> {
                f.read_at(&mut l2_bytes, l2_offset)?;
                Ok(l2_bytes)
            })
            .await
            .map_err(DiskError::Io)?;
            let mut l2_slice = l2_bytes.as_slice();
            let mut l2_table = read_l2_table(&mut l2_slice, l2_entries as u32)
                .map_err(|e| DiskError::Io(std::io::Error::other(e)))?;

            let l2_entry = &l2_table[addr.l2_index as usize];
            if l2_entry.compressed {
                return Err(DiskError::InvalidInput);
            }

            let data_cluster_offset = if l2_entry.cluster_offset == 0 {
                let cluster_size = cluster_size as u64;
                let new_cluster = allocate_cluster(file.clone(), cluster_size).await?;
                zero_cluster(file.clone(), new_cluster, cluster_size as usize).await?;
                state
                    .refcounts
                    .increment_cluster(&file, new_cluster / cluster_size)
                    .await?;
                new_cluster
            } else {
                l2_entry.cluster_offset
            };

            let buf_off = (byte_off - offset) as usize;
            if bytes_in_cluster < cluster_size {
                let mut full = vec![0u8; cluster_size];
                let f = file.clone();
                let full = unblock(move || -> Result<Vec<u8>, std::io::Error> {
                    f.read_at(&mut full, data_cluster_offset)?;
                    Ok(full)
                })
                .await
                .map_err(DiskError::Io)?;
                let mut full = full;
                buffers
                    .subrange(buf_off, bytes_in_cluster)
                    .reader()
                    .read(&mut full[addr.in_cluster_offset as usize..][..bytes_in_cluster])?;
                let f = file.clone();
                unblock(move || f.write_at(&full, data_cluster_offset))
                    .await
                    .map_err(DiskError::Io)?;
            } else {
                let mut data = vec![0u8; bytes_in_cluster];
                buffers
                    .subrange(buf_off, bytes_in_cluster)
                    .reader()
                    .read(&mut data)?;
                let f = file.clone();
                unblock(move || f.write_at(&data, data_cluster_offset))
                    .await
                    .map_err(DiskError::Io)?;
            }

            l2_table[addr.l2_index as usize].cluster_offset = data_cluster_offset;
            let f = file.clone();
            unblock(move || write_l2_table(&f, l2_offset, &l2_table))
                .await
                .map_err(DiskError::Io)?;

            byte_off += bytes_in_cluster as u64;
        }
        Ok(())
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
