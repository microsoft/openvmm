// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Qcow2 refcount table and block handling.
//!
//! Every host cluster in a qcow2 image has an associated reference count,
//! stored in a two-level structure managed by the qcow2 spec:

use blocking::unblock;
use disk_backend::DiskError;
use std::collections::HashMap;
use std::fs::File;
use std::sync::Arc;

use crate::header::Qcow2Header;
use crate::readwriteat::ReadWriteAt;

/// Maximum number of refcount blocks cached in memory before evicting all of
/// them. They are re-readable from disk, so eviction is always safe.
const MAX_CACHED_BLOCKS: usize = 64;

/// Allocate a fresh cluster at the end of the file, aligned to `cluster_size`,
/// and extend the file to hold it. Returns the offset of the new cluster.
pub async fn allocate_cluster(file: Arc<File>, cluster_size: u64) -> Result<u64, DiskError> {
    unblock(move || -> std::io::Result<u64> {
        let file_len = file.metadata()?.len();
        let new_offset = file_len.div_ceil(cluster_size) * cluster_size;
        file.set_len(new_offset + cluster_size)?;
        Ok(new_offset)
    })
    .await
    .map_err(DiskError::Io)
}

/// Zero out `size` bytes at the given (cluster-aligned) offset.
pub async fn zero_cluster(
    file: Arc<File>,
    cluster_offset: u64,
    size: usize,
) -> Result<(), DiskError> {
    let buf = vec![0u8; size];
    unblock(move || {
        let n = file.write_at(&buf, cluster_offset)?;
        if n != buf.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "short write",
            ));
        }
        Ok(())
    })
    .await
    .map_err(DiskError::Io)?;
    Ok(())
}

/// The refcount state of a qcow2 image.
///
/// Only mutated while holding the layer's state mutex, which serializes all
/// allocation, so the in-memory table and block cache stay consistent.
pub struct RefcountTable {
    table_offset: u64,
    /// One entry per refcount block: the block's offset in the image file, or
    /// 0 if the block has not been allocated yet.
    entries: Vec<u64>,
    /// Number of reference counts stored in a single refcount block.
    entries_per_block: u64,
    cluster_size: u64,
    /// Cache of loaded refcount blocks, keyed by refcount table index.
    blocks: HashMap<u64, Vec<u16>>,
}

impl RefcountTable {
    /// Build an empty (not yet loaded) refcount table from a parsed header.
    pub fn new(header: &Qcow2Header) -> anyhow::Result<Self> {
        let refcount_order = match &header.extended_version3_header {
            Some(v3) => v3.refcount_order,
            None => 4, // V2 images always use 16-bit refcount entries
        };
        anyhow::ensure!(refcount_order >= 3, "refcount_order too small");
        // Each entry is (1 << refcount_order) bits wide.
        let entry_bytes = 1u64 << (refcount_order - 3);
        Ok(Self {
            table_offset: header.refcount_table_offset,
            entries: Vec::new(),
            entries_per_block: header.cluster_size() / entry_bytes,
            cluster_size: header.cluster_size(),
            blocks: HashMap::new(),
        })
    }

    /// Populate the refcount table from the raw big-endian bytes read from the
    /// image's refcount table. Each entry is an 8-byte big-endian offset.
    pub fn set_table_bytes(&mut self, bytes: &[u8]) {
        self.entries = bytes
            .chunks_exact(8)
            .map(|c| u64::from_be_bytes(c.try_into().expect("8 bytes")))
            .collect();
    }

    /// Whether the image has a usable refcount table (needed for writes).
    pub fn is_available(&self) -> bool {
        self.table_offset != 0 && !self.entries.is_empty()
    }

    /// Increment the refcount of the given host cluster by one, allocating
    /// refcount blocks (and updating the refcount table) if the blocks covering
    /// this cluster do not exist yet.
    pub async fn increment_cluster(
        &mut self,
        file: &Arc<File>,
        cluster: u64,
    ) -> Result<(), DiskError> {
        if !self.is_available() {
            return Err(DiskError::Io(std::io::Error::other(
                "qcow2 image has no refcount table; cannot write safely",
            )));
        }

        // Allocating a refcount block also requires bumping that block's own
        // refcount, which in turn can require yet another block. Because every
        // block is allocated at the end of the file, this chain only moves
        // towards higher clusters, so it can be walked iteratively instead of
        // recursively.
        let mut to_increment = Vec::new();
        let mut cur = cluster;
        loop {
            let table_index = (cur / self.entries_per_block) as usize;
            if table_index >= self.entries.len() {
                // TODO: grow/relocate the refcount table and update the header
                // (which also requires adjusting the refcounts of the old
                // table clusters). Only reachable for images whose clusters
                // are no longer covered by the existing refcount table.
                return Err(DiskError::Io(std::io::Error::other(
                    "qcow2 refcount table is exhausted",
                )));
            }
            if self.entries[table_index] != 0 {
                break;
            }

            let block_offset = allocate_cluster(file.clone(), self.cluster_size).await?;
            zero_cluster(file.clone(), block_offset, self.cluster_size as usize).await?;
            self.entries[table_index] = block_offset;
            let f = file.clone();
            let entry_offset = self.table_offset + table_index as u64 * 8;
            unblock(move || -> std::io::Result<()> {
                let n = f.write_at(&block_offset.to_be_bytes(), entry_offset)?;
                if n != 8 {
                    return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "short write"));
                }
                Ok(())
            })
            .await
            .map_err(DiskError::Io)?;

            // The new block is itself referenced once, by the refcount table.
            to_increment.push(block_offset / self.cluster_size);
            cur = block_offset / self.cluster_size;
        }

        to_increment.push(cluster);
        for c in to_increment {
            self.bump_refcount(file, c).await?;
        }
        Ok(())
    }

    /// Bump the refcount of `cluster` by one. Requires that the refcount block
    /// covering `cluster` already exists.
    async fn bump_refcount(&mut self, file: &Arc<File>, cluster: u64) -> Result<(), DiskError> {
        let table_index = (cluster / self.entries_per_block) as usize;
        let in_block = (cluster % self.entries_per_block) as usize;
        let block_offset = self.entries[table_index];

        let counts = match self.blocks.get_mut(&(table_index as u64)) {
            Some(counts) => counts,
            None => {
                let counts = self.load_block(file, table_index).await?;
                self.blocks.insert(table_index as u64, counts);
                self.blocks.get_mut(&(table_index as u64)).unwrap()
            }
        };
        counts[in_block] = counts[in_block].saturating_add(1);

        let mut bytes = Vec::with_capacity(counts.len() * 2);
        for count in counts.iter() {
            bytes.extend_from_slice(&count.to_be_bytes());
        }
        let f = file.clone();
        unblock(move || f.write_at(&bytes, block_offset))
            .await
            .map_err(DiskError::Io)?;

        if self.blocks.len() > MAX_CACHED_BLOCKS {
            self.blocks.clear();
        }
        Ok(())
    }

    async fn load_block(
        &self,
        file: &Arc<File>,
        table_index: usize,
    ) -> Result<Vec<u16>, DiskError> {
        let block_offset = self.entries[table_index];
        let byte_len = (self.entries_per_block * 2) as usize;
        let mut buf = vec![0u8; byte_len];
        let f = file.clone();
        let buf = unblock(move || -> Result<Vec<u8>, std::io::Error> {
            let n = f.read_at(&mut buf, block_offset)?;
            if n != buf.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "short read",
                ));
            }
            Ok(buf)
        })
        .await
        .map_err(DiskError::Io)?;
        Ok(buf
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect())
    }
}
