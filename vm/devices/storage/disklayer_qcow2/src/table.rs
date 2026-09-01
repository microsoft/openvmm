// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Qcow2 L1 and L2 table entry layout.
//!
//! The qcow2 format maintains a two-level table used to map guest-visible
//! cluster addresses to physical offsets in the image file:
//!
//! - The **L1 table** has one [`L1Entry`] per top-level index. Each entry
//!   points to the offset of an L2 table in the image file.
//! - Each **L2 table** has one [`L2Entry`] per cluster index. Each entry
//!   points to the offset of a data cluster in the image file (or encodes a
//!   compressed cluster).

use crate::Qcow2Header;
use anyhow::Context;
use core::mem::size_of;
use inspect::Inspect;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;

/// Mask covering bits 9..=55 (47 bits), the offset field shared by L1 and L2
/// entries.
const OFFSET_MASK: u64 = ((1u64 << 47) - 1) << 9;

const OFLAG_COPIED: u64 = 1 << 63;

/// An entry in the active L1 table, Each entry describes one L2 table
/// If `l2_offset` is 0, the L2 table and all clusters it describes are
/// unallocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Inspect)]
pub struct L1Entry {
    /// Offset of the L2 table in the image file, aligned to a cluster boundary.
    pub l2_offset: u64,
    /// Whether the L2 table's refcount is exactly one (accurate only in the
    /// active L1 table).
    pub copied: bool,
}

/// An entry in an L2 table and Each entry describes one data cluster.
/// If `cluster_offset` is 0, the cluster is unallocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct L2Entry {
    /// Offset of the data cluster in the image file, aligned to a cluster
    /// boundary. For a compressed cluster this is the offset of the host
    pub cluster_offset: u64,
    /// Whether the cluster is stored compressed.
    pub compressed: bool,
    /// Whether the cluster's refcount is exactly one.
    pub copied: bool,
    /// For a compressed cluster, the sector offset within the host cluster.
    pub sector_offset_in_cluster: u64,
}

/// The result of splitting a guest byte offset into its three address components.
#[derive(Debug, Clone, Copy)]
pub struct ClusterAddress {
    pub l1_index: u64,
    pub l2_index: u64,
    pub in_cluster_offset: u64,
}

impl L1Entry {
    /// Decode an L1 table entry from its raw big-endian value.
    pub fn decode(raw: u64) -> anyhow::Result<Self> {
        let reserved = raw & !(OFFSET_MASK | (1 << 63));
        anyhow::ensure!(
            reserved == 0,
            "L1 table entry has non-zero reserved bits: {reserved:#x}"
        );
        Ok(Self {
            l2_offset: raw & OFFSET_MASK,
            copied: raw & (1 << 63) != 0,
        })
    }

    /// Read a single L1 table entry from a big-endian byte slice.
    pub fn read(input: &mut &[u8]) -> anyhow::Result<Self> {
        let raw = read_be_u64(input)?;
        Self::decode(raw)
    }
}

impl L2Entry {
    /// Decode an L2 table entry from its raw big-endian value.
    pub fn decode(raw: u64) -> anyhow::Result<Self> {
        let compressed = raw & (1 << 62) != 0;
        // Bits 0-8 are only meaningful for compressed clusters.
        let reserved_mask = if compressed {
            !(OFFSET_MASK | (1 << 9) - 1 | (1 << 62) | (1 << 63))
        } else {
            !(OFFSET_MASK | (1 << 62) | (1 << 63))
        };
        let reserved = raw & reserved_mask;
        anyhow::ensure!(
            reserved == 0,
            "L2 table entry has non-zero reserved bits: {reserved:#x}"
        );
        Ok(Self {
            cluster_offset: raw & OFFSET_MASK,
            compressed,
            copied: raw & (1 << 63) != 0,
            sector_offset_in_cluster: raw & ((1 << 9) - 1),
        })
    }

    /// Read a single L2 table entry from a big-endian byte slice.
    pub fn read(input: &mut &[u8]) -> anyhow::Result<Self> {
        let raw = read_be_u64(input)?;
        Self::decode(raw)
    }
}

fn read_be_u64(input: &mut &[u8]) -> anyhow::Result<u64> {
    anyhow::ensure!(
        input.len() >= size_of::<u64>(),
        "Input too short to read u64"
    );
    let (int_bytes, rest) = input.split_at(size_of::<u64>());
    *input = rest;

    Ok(u64::from_be_bytes(
        int_bytes
            .try_into()
            .context("failed to convert bytes to u64")?,
    ))
}

/// Read the full L1 table from the image header bytes.
pub fn read_l1_table(input: &mut &[u8], l1_size: u32) -> anyhow::Result<Vec<L1Entry>> {
    (0..l1_size)
        .map(|i| L1Entry::read(input).with_context(|| format!("failed to read L1 table entry {i}")))
        .collect()
}

/// Read a full L2 table from the image header bytes.
pub fn read_l2_table(input: &mut &[u8], l2_size: u32) -> anyhow::Result<Vec<L2Entry>> {
    (0..l2_size)
        .map(|i| L2Entry::read(input).with_context(|| format!("failed to read L2 table entry {i}")))
        .collect()
}

/// Split a guest logical offset into (L1 index, L2 index, in-cluster offset).
pub fn split_guest_offset(header: &Qcow2Header, guest_offset: u64) -> ClusterAddress {
    let cluster_bits = header.cluster_bits as u64;
    let l2_bits = header.l2_entries_per_table().trailing_zeros() as u64; // log2(entries per L2)

    let in_cluster_offset = guest_offset & (header.cluster_size() - 1);
    let l2_index = (guest_offset >> cluster_bits) & ((1 << l2_bits) - 1);
    let l1_index = guest_offset >> (cluster_bits + l2_bits);

    ClusterAddress {
        l1_index,
        l2_index,
        in_cluster_offset,
    }
}

/// Write an entire L1 table back to disk at `l1_offset`
pub fn write_l1_entry(
    file: &mut std::fs::File,
    header: &Qcow2Header,
    l1_index: u64,
    l2_offset: u64,
) -> std::io::Result<()> {
    let entry_offset = header.l1_table_offset + l1_index * 8;
    let raw = l2_offset | OFLAG_COPIED;
    file.seek(SeekFrom::Start(entry_offset))?;
    file.write_all(&raw.to_be_bytes())?;
    Ok(())
}

/// Write an entire L2 table back to disk at `l2_offset`.
pub fn write_l2_table(
    file: &mut std::fs::File,
    l2_offset: u64,
    l2_table: &[L2Entry],
) -> std::io::Result<()> {
    let mut bytes = Vec::with_capacity(l2_table.len() * 8);
    for entry in l2_table {
        if entry.compressed {
            return Err(std::io::Error::other(
                "writing back compressed L2 entries is not supported",
            ));
        }
        let raw = if entry.cluster_offset == 0 {
            0
        } else {
            entry.cluster_offset | OFLAG_COPIED
        };
        bytes.extend_from_slice(&raw.to_be_bytes());
    }
    file.seek(SeekFrom::Start(l2_offset))?;
    file.write_all(&bytes)?;
    Ok(())
}
