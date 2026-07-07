// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtio block device specification constants and types.
//!
//! Based on OASIS VIRTIO v1.2, Section 5.2.
//! <https://docs.oasis-open.org/virtio/virtio/v1.2/cs01/virtio-v1.2-cs01.html>

use inspect::Inspect;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

// Feature bits (spec §5.2.3). These are device-specific bits in bank 0 (bits 0..23).
/// Maximum size of any single segment is in `size_max`.
pub const VIRTIO_BLK_F_SIZE_MAX: u32 = 1 << 1;
/// Maximum number of segments in a request is in `seg_max`.
pub const VIRTIO_BLK_F_SEG_MAX: u32 = 1 << 2;
/// Device is read-only.
pub const VIRTIO_BLK_F_RO: u32 = 1 << 5;
/// Block size of disk is in `blk_size`.
pub const VIRTIO_BLK_F_BLK_SIZE: u32 = 1 << 6;
/// Cache flush command support.
pub const VIRTIO_BLK_F_FLUSH: u32 = 1 << 9;
/// Device exports information on optimal I/O alignment.
pub const VIRTIO_BLK_F_TOPOLOGY: u32 = 1 << 10;
/// Device can support discard command.
pub const VIRTIO_BLK_F_DISCARD: u32 = 1 << 13;
/// Device can support write zeroes command (not currently advertised).
pub const VIRTIO_BLK_F_WRITE_ZEROES: u32 = 1 << 14;

// Request types (spec §5.2.6).
pub const VIRTIO_BLK_T_IN: u32 = 0;
pub const VIRTIO_BLK_T_OUT: u32 = 1;
pub const VIRTIO_BLK_T_FLUSH: u32 = 4;
pub const VIRTIO_BLK_T_GET_ID: u32 = 8;
pub const VIRTIO_BLK_T_DISCARD: u32 = 11;
pub const VIRTIO_BLK_T_WRITE_ZEROES: u32 = 13;

// Status codes (spec §5.2.6).
pub const VIRTIO_BLK_S_OK: u8 = 0;
pub const VIRTIO_BLK_S_IOERR: u8 = 1;
pub const VIRTIO_BLK_S_UNSUPP: u8 = 2;

/// Maximum length of the device ID string (spec §5.2.6).
pub const VIRTIO_BLK_ID_BYTES: usize = 20;

/// Flag bit in `VirtioBlkDiscardWriteZeroes::flags` (spec §5.2.6).
/// When set in a write zeroes command, allows the device to deallocate
/// the range instead of (or in addition to) zeroing it, as long as
/// subsequent reads still return zeroes.
pub const VIRTIO_BLK_WRITE_ZEROES_FLAG_UNMAP: u32 = 1;

/// Virtio block device config space layout (spec §5.2.4).
///
/// All multi-byte fields are little-endian.
/// Fields are only valid when their corresponding feature bit is negotiated.
#[repr(C)]
#[derive(Debug, Clone, IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
pub struct VirtioBlkConfig {
    /// Capacity in 512-byte sectors (always present).
    pub capacity: u64,
    /// Maximum size (in bytes) of any single segment in a request
    /// (valid if VIRTIO_BLK_F_SIZE_MAX).
    pub size_max: u32,
    /// Maximum number of segments in a request
    /// (valid if VIRTIO_BLK_F_SEG_MAX).
    pub seg_max: u32,
    /// CHS geometry (valid if VIRTIO_BLK_F_GEOMETRY). Unused — we report 0.
    pub geometry: VirtioBlkGeometry,
    /// Logical block size in bytes (valid if VIRTIO_BLK_F_BLK_SIZE).
    /// Does not change protocol units (always 512 bytes) but informs the
    /// driver of the optimal I/O alignment.
    pub blk_size: u32,
    /// Topology info (valid if VIRTIO_BLK_F_TOPOLOGY).
    pub topology: VirtioBlkTopology,
    /// Cache mode: 0=writethrough, 1=writeback
    /// (valid if VIRTIO_BLK_F_CONFIG_WCE, which we don't negotiate).
    pub writeback: u8,
    pub unused0: u8,
    /// Number of request queues (valid if VIRTIO_BLK_F_MQ,
    /// which we don't negotiate). Set to 1.
    pub num_queues: u16,
    /// Maximum number of 512-byte sectors in a single discard segment
    /// (valid if VIRTIO_BLK_F_DISCARD).
    pub max_discard_sectors: u32,
    /// Maximum number of discard segments per request
    /// (valid if VIRTIO_BLK_F_DISCARD). We support only 1.
    pub max_discard_seg: u32,
    /// Required alignment for discard ranges, in 512-byte sectors
    /// (valid if VIRTIO_BLK_F_DISCARD). Set to the disk's logical
    /// sector size expressed in 512-byte units.
    pub discard_sector_alignment: u32,
    /// Maximum number of 512-byte sectors in a single write zeroes segment
    /// (valid if VIRTIO_BLK_F_WRITE_ZEROES).
    pub max_write_zeroes_sectors: u32,
    /// Maximum number of write zeroes segments per request
    /// (valid if VIRTIO_BLK_F_WRITE_ZEROES). We support only 1.
    pub max_write_zeroes_seg: u32,
    /// If nonzero, a write zeroes command MAY result in deallocation of
    /// the range (when the unmap flag is set). Set to 1 only if the
    /// backend's unmap behavior guarantees zeroes
    /// (valid if VIRTIO_BLK_F_WRITE_ZEROES).
    pub write_zeroes_may_unmap: u8,
    #[inspect(skip)]
    pub unused1: [u8; 3],
    // Explicit padding to satisfy zerocopy IntoBytes alignment requirements
    // for the u64 `capacity` field. Not part of the virtio config space;
    // `device_register_length` excludes these bytes.
    #[inspect(skip)]
    pub _padding: [u8; 4],
}

#[repr(C)]
#[derive(Debug, Clone, IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
pub struct VirtioBlkGeometry {
    pub cylinders: u16,
    pub heads: u8,
    pub sectors: u8,
}

#[repr(C)]
#[derive(Debug, Clone, IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
pub struct VirtioBlkTopology {
    /// log2 of physical_block_size / logical_block_size
    pub physical_block_exp: u8,
    /// Offset of first aligned logical block.
    pub alignment_offset: u8,
    /// Suggested minimum I/O size in blocks.
    pub min_io_size: u16,
    /// Optimal (suggested maximum) I/O size in blocks.
    pub opt_io_size: u32,
}

/// Request header, read from the first descriptor.
#[repr(C)]
#[derive(Debug, Clone, IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
pub struct VirtioBlkReqHeader {
    pub request_type: u32,
    pub reserved: u32,
    pub sector: u64,
}

/// Discard/write zeroes data segment.
#[repr(C)]
#[derive(Debug, Clone, IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
pub struct VirtioBlkDiscardWriteZeroes {
    pub sector: u64,
    pub num_sectors: u32,
    pub flags: u32,
}
