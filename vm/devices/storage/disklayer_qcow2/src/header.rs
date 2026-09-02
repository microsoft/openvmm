// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use core::mem::size_of;
use inspect::Inspect;
use std::io::Read;

const QCOW2_MAGIC: u32 = 0x514649FB; // "QFI\xfb" as a big-endian u32

/// The standard header, defines values used by both V2 and V3
#[derive(Debug, Clone, Inspect)]
pub struct Qcow2Header {
    /// Version number (valid values are 2 and 3)
    pub header_version: u32,
    /// Offset into the image file at which the backing file name is stored
    pub backing_offset: u64,
    /// Length of the backing file name in bytes. Must not be longer than 1023 bytes
    pub backing_file_size: u32,
    /// Number of bits that are used for addressing an offset within a cluster
    pub cluster_bits: u32,
    /// Virtual disk size in bytes.
    pub size_bytes: u64,
    /// 0 for no encryption, 1 for AES encryption and 2 for LUKS encryption
    pub crypt_method: u32,
    /// Number of entries in the active L1 table
    pub l1_size: u32,
    /// Offset into the image file at which the active L1 table starts. Must be aligned to a cluster boundary.
    pub l1_table_offset: u64,
    /// Offset into the image file at which the refcount table starts. Must be aligned to a cluster boundary.
    pub refcount_table_offset: u64,
    /// Number of clusters that the refcount table occupies
    pub refcount_table_clusters: u32,
    /// Number of snapshots contained in the image
    pub nb_snapshots: u32,
    /// Offset into the image file at which the snapshot table starts. Must be aligned to a cluster boundary.
    pub snapshots_offset: u64,
    /// Bitmask of incompatible features (only present for version 3). Bits 0
    /// and 1 are the dirty and corrupt flags: both may be opened read-only,
    /// but only the dirty flag may be repaired and a corrupt image must not be
    /// written to. Any other (unknown) bit set is rejected at parse time.
    pub incompatible_features: Option<u64>,
    /// The Version 3 extra values
    pub extended_version3_header: Option<QcowV3Header>,
}

/// An extended version of the standard header, has the extra fields for V3
#[derive(Debug, Clone, Inspect)]
pub struct QcowV3Header {
    /// Bitmask of incompatible features. An implementation must fail to open an image if an unknown bit is set
    pub incompatible_features: u64,
    /// Bitmask of compatible features. An implementation can safely ignore any unknown bits that are set
    pub compatible_features: u64,
    /// Bitmask of auto-clear features. An implementation may only write to an image with unknown auto-clear
    /// features if it clears the respective bits from this field first
    pub autoclear_features: u64,
    /// Describes the width of a reference count block entry
    /// For version 2 images, the order is always assumed to be 4
    pub refcount_order: u32,
    /// Length of the header structure in bytes
    /// For version 2 images, the length is always assumed to be 72 bytes
    pub header_length: u32,
    /// Defines the compression method used for compressed clusters. All compressed clusters in an image
    /// use the same compression type
    pub compression_type: Option<u8>,
}

fn read_be_u8(input: &mut &[u8]) -> anyhow::Result<u8> {
    anyhow::ensure!(!input.is_empty(), "Input too short to read u8");

    let (int_bytes, rest) = input.split_at(size_of::<u8>());
    *input = rest;

    Ok(u8::from_be_bytes([int_bytes[0]]))
}

fn read_be_u32(input: &mut &[u8]) -> anyhow::Result<u32> {
    anyhow::ensure!(input.len() >= 4, "Input too short to read u32");

    let (int_bytes, rest) = input.split_at(size_of::<u32>());
    *input = rest;

    Ok(u32::from_be_bytes([
        int_bytes[0],
        int_bytes[1],
        int_bytes[2],
        int_bytes[3],
    ]))
}

fn read_be_u64(input: &mut &[u8]) -> anyhow::Result<u64> {
    anyhow::ensure!(input.len() >= 8, "Input too short to read u64");
    let (int_bytes, rest) = input.split_at(size_of::<u64>());
    *input = rest;

    Ok(u64::from_be_bytes([
        int_bytes[0],
        int_bytes[1],
        int_bytes[2],
        int_bytes[3],
        int_bytes[4],
        int_bytes[5],
        int_bytes[6],
        int_bytes[7],
    ]))
}

impl Qcow2Header {
    /// Read from an Open File into a `Qcow2` Header.
    ///
    /// The function has built in checks and limits, but these are only for what is defined in
    /// the spec, not what this implementation requires.
    pub fn from_file(file: &mut std::fs::File) -> anyhow::Result<Self> {
        let mut header = [0u8; 72]; // Length of V2 Header
        file.read_exact(&mut header)
            .context("failed to read qcow2 header")?;
        let header = &mut header.as_slice();

        let cluster_bits;
        Self::read_magic_number(header)?;
        let mut this = Self {
            header_version: Self::read_version_field(header)?,
            backing_offset: read_be_u64(header)?,
            backing_file_size: Self::read_backing_file_size(header)?,
            cluster_bits: {
                cluster_bits = Self::read_cluster_bits(header)?;
                cluster_bits
            },
            size_bytes: Self::read_disk_size(header)?,
            crypt_method: read_be_u32(header)?,
            l1_size: read_be_u32(header)?,
            l1_table_offset: Self::read_u64_cluster_aligned(header, cluster_bits)?,
            refcount_table_offset: Self::read_u64_cluster_aligned(header, cluster_bits)?,
            refcount_table_clusters: read_be_u32(header)?,
            nb_snapshots: read_be_u32(header)?,
            snapshots_offset: Self::read_u64_cluster_aligned(header, cluster_bits)?,
            incompatible_features: None,
            extended_version3_header: None,
        };

        if this.header_version == 3 {
            let mut header = [0u8; 32]; // Length of rest of V3 Header
            file.read_exact(&mut header)
                .context("failed to read qcow2 header")?;
            let header = &mut header.as_slice();

            let mut extended_version3_header = QcowV3Header {
                incompatible_features: read_be_u64(header)?,
                compatible_features: read_be_u64(header)?,
                autoclear_features: read_be_u64(header)?,
                refcount_order: Self::read_refcount_order(header)?,
                header_length: read_be_u32(header)?,
                compression_type: None,
            };
            anyhow::ensure!(
                extended_version3_header.header_length >= 104
                    && extended_version3_header.header_length.is_multiple_of(8),
                "invalid v3 header_length {} (must be >= 104 and a multiple of 8)",
                extended_version3_header.header_length
            );
            if extended_version3_header.header_length > 104 {
                let mut header = [0u8; 1]; // Length of rest of V3 Header
                file.read_exact(&mut header)
                    .context("failed to read qcow2 header")?;
                let header = &mut header.as_slice();

                extended_version3_header.compression_type = Some(read_be_u8(header)?);

                // Skip any additional header extension bytes so the whole
                // header (up to `header_length`) is consumed.
                let extension_len = extended_version3_header.header_length as usize - 105;
                if extension_len > 0 {
                    let mut skip = vec![0u8; extension_len];
                    file.read_exact(&mut skip)
                        .context("failed to read qcow2 header extension bytes")?;
                }
            }
            this.incompatible_features = Some(extended_version3_header.incompatible_features);
            this.extended_version3_header = Some(extended_version3_header);
        }

        anyhow::ensure!(
            this.crypt_method == 0,
            "encrypted qcow2 images are not supported"
        );
        anyhow::ensure!(
            this.backing_file_size == 0 && this.backing_offset == 0,
            "qcow2 backing files are not supported"
        );
        anyhow::ensure!(
            this.nb_snapshots == 0 && this.snapshots_offset == 0,
            "qcow2 snapshots are not supported"
        );
        if let Some(v3) = &this.extended_version3_header {
            // Bits 0 and 1 (dirty and corrupt) are understood; they are handled
            // by the layer, which restricts writes on a corrupt image. Unknown
            // bits must be rejected.
            let known = 0x3u64;
            anyhow::ensure!(
                v3.incompatible_features & !known == 0,
                "qcow2 unknown incompatible_features {:#x} are not supported",
                v3.incompatible_features & !known
            );
            anyhow::ensure!(
                v3.refcount_order == 4,
                "qcow2 refcount_order {} is not supported (only 4 / 16-bit)",
                v3.refcount_order
            );
        }

        let header_len = match &this.extended_version3_header {
            Some(v3) => v3.header_length as u64,
            None => 72,
        };
        anyhow::ensure!(
            this.l1_table_offset >= header_len,
            "l1_table_offset must not overlap the header"
        );

        Ok(this)
    }

    fn read_magic_number(header: &mut &[u8]) -> anyhow::Result<()> {
        let magic = read_be_u32(header)?;
        anyhow::ensure!(magic == QCOW2_MAGIC, "qcow2 magic number malformed");
        Ok(())
    }

    fn read_version_field(header: &mut &[u8]) -> anyhow::Result<u32> {
        let version = read_be_u32(header)?;
        anyhow::ensure!(version == 2 || version == 3, "Unsupported qcow2 version");
        Ok(version)
    }

    fn read_backing_file_size(header: &mut &[u8]) -> anyhow::Result<u32> {
        let file_size = read_be_u32(header)?;
        anyhow::ensure!(
            file_size <= 1023,
            "Backing file size too long for a qcow2 image"
        );
        Ok(file_size)
    }

    fn read_cluster_bits(header: &mut &[u8]) -> anyhow::Result<u32> {
        let cluster_bits = read_be_u32(header)?;
        anyhow::ensure!(
            (9..=21).contains(&cluster_bits),
            "Cluster bits must be between 9 and 21 for qcow2"
        );
        Ok(cluster_bits)
    }

    fn read_u64_cluster_aligned(header: &mut &[u8], cluster_bits: u32) -> anyhow::Result<u64> {
        let value = read_be_u64(header)?;
        anyhow::ensure!(
            value % (1u64 << cluster_bits) == 0,
            "Value must lie on a cluster boundary"
        );
        Ok(value)
    }

    fn read_disk_size(header: &mut &[u8]) -> anyhow::Result<u64> {
        let size = read_be_u64(header)?;
        anyhow::ensure!(size > 0, "qcow2 header reports an empty disk");
        Ok(size)
    }

    fn read_refcount_order(header: &mut &[u8]) -> anyhow::Result<u32> {
        let refcount_order = read_be_u32(header)?;
        anyhow::ensure!(
            refcount_order <= 6,
            "refcount_order may not exceed 6 in the qcow2 header"
        );
        Ok(refcount_order)
    }

    /// Total size of each cluster
    pub fn cluster_size(&self) -> u64 {
        1 << self.cluster_bits
    }

    /// Number of 8-byte entries in one L2 table (an L2 table is exactly one cluster).
    pub fn l2_entries_per_table(&self) -> u64 {
        self.cluster_size() / 8
    }
}
