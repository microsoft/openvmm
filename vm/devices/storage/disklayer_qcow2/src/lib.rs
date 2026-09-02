// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Qcow2 disk layer for OpenVMM.
//!
//! Provides a cross-platform, pure-Rust qcow2 backend for the layered disk
//! stack.
//!

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
/// Currently supports basic qcow2 v2/v3 images without encryption, backing files,
/// snapshots, or compressed clusters, and implements sparse reads and in-place writes.
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
    pub fn new(file: File, header: Qcow2Header, read_only: bool) -> anyhow::Result<Self> {
        // A corrupt image (incompatible bit 1) must not be written to. The
        // dirty flag (bit 0) implies refcounts may be inconsistent, which we
        // do not repair; treat it as requiring read-only access too.
        if header.incompatible_features.unwrap_or(0) & 0x3 != 0 && !read_only {
            anyhow::bail!("refusing to write to a dirty/corrupt qcow2 image");
        }
        if !read_only {
            if let Some(v3) = &header.extended_version3_header {
                anyhow::ensure!(
                    v3.autoclear_features == 0,
                    "qcow2 autoclear_features {:#x} are not supported for write access",
                    v3.autoclear_features
                );
            }
        }

        // The guest-visible size must be a whole number of 512-byte sectors,
        // otherwise the final partial sector would be silently inaccessible.
        anyhow::ensure!(
            header.size_bytes.is_multiple_of(SECTOR_SIZE as u64),
            "qcow2 disk size {} is not a multiple of the sector size",
            header.size_bytes
        );
        let sector_count = header.size_bytes / SECTOR_SIZE as u64;

        // Bound header-controlled sizes before allocating, so a crafted image
        // cannot request an unbounded allocation. The L1 table must have
        // enough entries to cover the virtual disk, but not vastly more.
        let l1_entries_needed = header
            .size_bytes
            .div_ceil(header.cluster_size() * header.l2_entries_per_table());
        anyhow::ensure!(
            header.l1_size as u64 >= l1_entries_needed,
            "qcow2 l1_size {} is too small to address a {} byte disk",
            header.l1_size,
            header.size_bytes
        );
        // Each L1 table entry is 8 bytes. A 2^63-byte disk (the max qcow2
        // allows) with 2 MiB clusters needs at most 16 Mi entries, i.e. 128 MiB
        // of table. Cap the allocation there so a hostile header can't OOM us.
        const MAX_L1_ENTRIES: u64 = 16 * 1024 * 1024;
        anyhow::ensure!(
            header.l1_size as u64 <= MAX_L1_ENTRIES,
            "qcow2 l1_size {} exceeds the supported maximum of {MAX_L1_ENTRIES}",
            header.l1_size
        );

        let mut l1_bytes = vec![0u8; header.l1_size as usize * 8];
        let n = file.read_at(&mut l1_bytes, header.l1_table_offset)?;
        if n != l1_bytes.len() {
            anyhow::bail!("short read while reading the qcow2 L1 table");
        }
        let mut l1_slice = l1_bytes.as_slice();
        let l1_table = read_l1_table(&mut l1_slice, header.l1_size)?;
        let cluster_size = header.cluster_size();
        for (i, entry) in l1_table.iter().enumerate() {
            anyhow::ensure!(
                entry.l2_offset == 0 || entry.l2_offset.is_multiple_of(cluster_size),
                "qcow2 L1 entry {i} has a non-cluster-aligned L2 offset {:#x}",
                entry.l2_offset
            );
        }
        let mut refcounts = RefcountTable::new(&header)?;
        if header.refcount_table_offset != 0 && header.refcount_table_clusters != 0 {
            // Bound the refcount table allocation so a hostile header can't
            // request an unbounded allocation. Cap it at the amount needed to
            // cover every refcount block for all clusters the L1 table can
            // reference, with an absolute ceiling as a hard backstop.
            let max_clusters = header.l1_size as u64 * header.l2_entries_per_table();
            let refcount_entries_per_block = header.cluster_size() / 2; // 16-bit refcounts
            let table_entries_needed = max_clusters.div_ceil(refcount_entries_per_block);
            let table_clusters_needed = table_entries_needed.div_ceil(header.cluster_size() / 8);

            const MAX_REFCOUNT_TABLE_BYTES: u64 = 16 * 1024 * 1024; // hard cap to avoid OOM on crafted images
            let table_bytes_len_u64 = (header.refcount_table_clusters as u64)
                .checked_mul(header.cluster_size())
                .ok_or_else(|| anyhow::anyhow!("qcow2 refcount table length overflow"))?;
            anyhow::ensure!(
                header.refcount_table_clusters as u64 <= table_clusters_needed.max(1)
                    && table_bytes_len_u64 <= MAX_REFCOUNT_TABLE_BYTES,
                "qcow2 refcount table of {} bytes is unreasonably large",
                table_bytes_len_u64
            );

            let table_bytes_len: usize = table_bytes_len_u64
                .try_into()
                .map_err(|_| anyhow::anyhow!("qcow2 refcount table is too large to allocate"))?;
            let mut table_bytes = vec![0u8; table_bytes_len];
            let n = file.read_at(&mut table_bytes, header.refcount_table_offset)?;
            if n != table_bytes.len() {
                anyhow::bail!("short read while reading the qcow2 refcount table");
            }
            refcounts.set_table_bytes(&table_bytes);
        }

        // Writes require a usable refcount table; fail eagerly rather than at
        // the first write.
        if !refcounts.is_available() && !read_only {
            anyhow::bail!("qcow2 image has no refcount table; cannot write safely");
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

    /// Validate that the request at `sector` for `byte_len` bytes lies wholly
    /// within the layer's disk and is sector-aligned.
    fn validate_range(&self, sector: u64, byte_len: usize) -> Result<(), DiskError> {
        if !byte_len.is_multiple_of(SECTOR_SIZE as usize) {
            return Err(DiskError::InvalidInput);
        }
        let end_sector = sector + byte_len as u64 / SECTOR_SIZE as u64;
        if end_sector > self.sector_count {
            return Err(DiskError::IllegalBlock);
        }
        Ok(())
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
        self.validate_range(sector, len)?;
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
                // Unallocated in this layer: do not mark sectors as present so lower layers
                // (or LayeredDisk's final zero-fill) can provide the data.
                let skip_n = min(
                    (end - byte_off) as usize,
                    cluster_size - addr.in_cluster_offset as usize,
                );
                byte_off += skip_n as u64;
                continue;
            }

            // TODO: Add L2 table caching for performance.
            let l2_table_offset = l1_entry.l2_offset;
            let mut l2_bytes = vec![0u8; l2_entries * 8];
            let f = file.clone();
            let l2_bytes = unblock(move || -> Result<Vec<u8>, std::io::Error> {
                let n = f.read_at(&mut l2_bytes, l2_table_offset)?;
                if n != l2_bytes.len() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "short read",
                    ));
                }
                Ok(l2_bytes)
            })
            .await
            .map_err(DiskError::Io)?;
            let mut l2_slice = l2_bytes.as_slice();
            let l2_table = read_l2_table(&mut l2_slice, l2_entries as u32)
                .map_err(|e| DiskError::Io(std::io::Error::other(e)))?;
            let l2_entry: &L2Entry = &l2_table[addr.l2_index as usize];

            if l2_entry.compressed {
                return Err(DiskError::InvalidInput);
            }
            if l2_entry.cluster_offset != 0
                && !l2_entry.reads_as_zeros
                && l2_entry.cluster_offset % cluster_size as u64 != 0
            {
                return Err(DiskError::InvalidInput);
            }

            if l2_entry.reads_as_zeros {
                // Marked as reading as all zeros; zero the covered portion of the request.
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

            if l2_entry.cluster_offset == 0 {
                // Unallocated in this layer: do not mark sectors so lower layers (or final zero-fill)
                // can provide the data.
                let skip_n = min(
                    (end - byte_off) as usize,
                    cluster_size - addr.in_cluster_offset as usize,
                );
                byte_off += skip_n as u64;
                continue;
            }

            let bytes_in_cluster = min(
                (end - byte_off) as usize,
                cluster_size - addr.in_cluster_offset as usize,
            );
            let file_offset = l2_entry.cluster_offset + addr.in_cluster_offset;

            let mut data = vec![0u8; bytes_in_cluster];
            let f = file.clone();
            let data = unblock(move || -> Result<Vec<u8>, std::io::Error> {
                let n = f.read_at(&mut data, file_offset)?;
                if n != data.len() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "short read",
                    ));
                }
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
        if self.read_only {
            return Err(DiskError::ReadOnly);
        }

        let offset = sector * SECTOR_SIZE as u64;
        let len = buffers.len();
        self.validate_range(sector, len)?;
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
                let n = f.read_at(&mut l2_bytes, l2_offset)?;
                if n != l2_bytes.len() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "short read",
                    ));
                }
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

            let needs_allocation = l2_entry.cluster_offset == 0;
            let data_cluster_offset = if needs_allocation {
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
                if !needs_allocation {
                    let f = file.clone();
                    full = unblock(move || -> Result<Vec<u8>, std::io::Error> {
                        let n = f.read_at(&mut full, data_cluster_offset)?;
                        if n != full.len() {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::UnexpectedEof,
                                "short read",
                            ));
                        }
                        Ok(full)
                    })
                    .await
                    .map_err(DiskError::Io)?;
                }
                buffers
                    .subrange(buf_off, bytes_in_cluster)
                    .reader()
                    .read(&mut full[addr.in_cluster_offset as usize..][..bytes_in_cluster])?;
                let f = file.clone();
                unblock(move || -> std::io::Result<()> {
                    let n = f.write_at(&full, data_cluster_offset)?;
                    if n != full.len() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::WriteZero,
                            "short write",
                        ));
                    }
                    Ok(())
                })
                .await
                .map_err(DiskError::Io)?;
            } else {
                let mut data = vec![0u8; bytes_in_cluster];
                buffers
                    .subrange(buf_off, bytes_in_cluster)
                    .reader()
                    .read(&mut data)?;
                let f = file.clone();
                unblock(move || -> std::io::Result<()> {
                    let n = f.write_at(&data, data_cluster_offset)?;
                    if n != data.len() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::WriteZero,
                            "short write",
                        ));
                    }
                    Ok(())
                })
                .await
                .map_err(DiskError::Io)?;
            }

            if needs_allocation {
                // A freshly allocated cluster has refcount 1, so its COPIED
                // bit is set.
                l2_table[addr.l2_index as usize].copied = true;
            }
            l2_table[addr.l2_index as usize].cluster_offset = data_cluster_offset;
            l2_table[addr.l2_index as usize].reads_as_zeros = false;
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
        // TODO: qcow2 unmap (deallocating host clusters and clearing the
        // corresponding L2 entries) is not implemented yet. Report it as
        // unsupported input rather than an I/O error.
        Err(DiskError::InvalidInput)
    }
}
