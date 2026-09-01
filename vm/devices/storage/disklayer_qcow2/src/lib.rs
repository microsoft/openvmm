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
#[cfg(test)]
mod tests;

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
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::sync::RwLock;

use crate::header::Qcow2Header;
use crate::table::ClusterAddress;
use crate::table::L1Entry;
use crate::table::L2Entry;
use crate::table::read_l1_table;
use crate::table::read_l2_table;
use crate::table::split_guest_offset;
use crate::table::write_l1_entry;
use crate::table::write_l2_table;

const SECTOR_SIZE: u32 = 512;

/// A qcow2 disk layer implementing [`LayerIo`].
///
/// NOTE: This is a placeholder backing layer. Actual qcow2 format parsing
/// (header, L1/L2 tables, cluster allocation, backing file handling) is not
/// yet implemented.
#[derive(Inspect)]
pub struct Qcow2Layer {
    #[inspect(skip)]
    file: File,
    header: Qcow2Header,
    sector_count: u64,
    read_only: bool,
    #[inspect(skip)]
    l1_table: RwLock<Vec<L1Entry>>,
}

impl Qcow2Layer {
    /// Create a `Qcow2Layer` from an open file.
    ///
    /// NOTE: `sector_count` is passed by the caller until format parsing is
    /// implemented.
    pub fn new(mut file: File, header: Qcow2Header, read_only: bool) -> anyhow::Result<Self> {
        let sector_count = header.size_bytes / SECTOR_SIZE as u64;

        file.seek(SeekFrom::Start(header.l1_table_offset))?;
        let mut l1_bytes = vec![0u8; header.l1_size as usize * 8];
        file.read_exact(&mut l1_bytes)?;
        let mut l1_slice = l1_bytes.as_slice();
        let l1_table = read_l1_table(&mut l1_slice, header.l1_size)?;

        Ok(Self {
            file,
            header,
            sector_count,
            read_only,
            l1_table: RwLock::new(l1_table),
        })
    }

    fn zero_cluster(
        &self,
        file: &mut File,
        cluster_offset: u64,
        size: usize,
    ) -> Result<(), DiskError> {
        let buf = vec![0u8; size as usize];
        file.seek(SeekFrom::Start(cluster_offset))
            .map_err(DiskError::Io)?;
        file.write(buf.as_slice()).map_err(DiskError::Io)?;
        Ok(())
    }

    fn allocate_cluster(&self, file: &mut File) -> Result<u64, DiskError> {
        let cluster_size = self.header.cluster_size() as u64;

        let file_len = file.seek(SeekFrom::End(0)).map_err(DiskError::Io)?;
        let new_offset = ((file_len + cluster_size - 1) / cluster_size) * cluster_size;

        file.set_len(new_offset + cluster_size)
            .map_err(DiskError::Io)?;

        Ok(new_offset)
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
        let offset = sector * SECTOR_SIZE as u64;
        let len = buffers.len();
        let cluster_size = self.header.cluster_size() as usize;
        let l2_entries = self.header.l2_entries_per_table() as usize;

        let mut file = self.file.try_clone().map_err(DiskError::Io)?;
        let l1_table = self.l1_table.read().unwrap(); // If this needs errors, the thread is already panicing

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
            let mut l2_bytes = vec![0u8; l2_entries * 8];
            file.seek(SeekFrom::Start(l1_entry.l2_offset))
                .map_err(DiskError::Io)?;
            file.read_exact(&mut l2_bytes).map_err(DiskError::Io)?;
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
            file.seek(SeekFrom::Start(file_offset))
                .map_err(DiskError::Io)?;
            file.read_exact(&mut data).map_err(DiskError::Io)?;

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
        let mut file = self.file.try_clone().map_err(DiskError::Io)?;
        let mut l1_table = self.l1_table.write().unwrap(); // If this needs errors, the thread is already panicing
        let mut byte_off = offset;
        let end = offset + len as u64;

        while byte_off < end {
            let addr: ClusterAddress = split_guest_offset(&self.header, byte_off);
            if addr.l1_index as usize >= l1_table.len() {
                return Err(DiskError::IllegalBlock);
            }

            let bytes_in_cluster = min(
                (end - byte_off) as usize,
                cluster_size - addr.in_cluster_offset as usize,
            );

            let l2_offset = l1_table[addr.l1_index as usize].l2_offset;
            let l2_offset = if l2_offset == 0 {
                let new_l2 = self.allocate_cluster(&mut file)?;
                self.zero_cluster(&mut file, new_l2, cluster_size)?;

                l1_table[addr.l1_index as usize].l2_offset = new_l2;
                write_l1_entry(&mut file, &self.header, addr.l1_index, new_l2)
                    .map_err(DiskError::Io)?;

                new_l2
            } else {
                l2_offset
            };

            // TODO: Read cache for Level 2 entries
            let mut l2_bytes = vec![0u8; l2_entries * 8];
            file.seek(SeekFrom::Start(l2_offset))
                .map_err(DiskError::Io)?;
            file.read_exact(&mut l2_bytes).map_err(DiskError::Io)?;
            let mut l2_slice = l2_bytes.as_slice();
            let mut l2_table = read_l2_table(&mut l2_slice, l2_entries as u32)
                .map_err(|e| DiskError::Io(std::io::Error::other(e)))?;

            let l2_entry = &l2_table[addr.l2_index as usize];
            if l2_entry.compressed {
                return Err(DiskError::InvalidInput);
            }

            let data_cluster_offset = if l2_entry.cluster_offset == 0 {
                let new_cluster = self.allocate_cluster(&mut file)?;
                self.zero_cluster(&mut file, new_cluster, cluster_size)?;
                new_cluster
            } else {
                // TODO once refcounts exist: if this cluster's refcount > 1
                // (shared with a backing file / snapshot), COW-duplicate it
                // here instead of writing in place.
                l2_entry.cluster_offset
            };

            let buf_off = (byte_off - offset) as usize;
            if bytes_in_cluster < cluster_size {
                let mut full = vec![0u8; cluster_size];
                file.seek(SeekFrom::Start(data_cluster_offset))
                    .map_err(DiskError::Io)?;
                file.read_exact(&mut full).map_err(DiskError::Io)?;
                buffers
                    .subrange(buf_off, bytes_in_cluster)
                    .reader()
                    .read(&mut full[addr.in_cluster_offset as usize..][..bytes_in_cluster])?;
                file.seek(SeekFrom::Start(data_cluster_offset))
                    .map_err(DiskError::Io)?;
                file.write_all(&full).map_err(DiskError::Io)?;
            } else {
                let mut data = vec![0u8; bytes_in_cluster];
                buffers
                    .subrange(buf_off, bytes_in_cluster)
                    .reader()
                    .read(&mut data)?;
                file.seek(SeekFrom::Start(data_cluster_offset))
                    .map_err(DiskError::Io)?;
                file.write_all(&data).map_err(DiskError::Io)?;
            }

            l2_table[addr.l2_index as usize].cluster_offset = data_cluster_offset;
            write_l2_table(&mut file, l2_offset, &l2_table).map_err(DiskError::Io)?;

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
