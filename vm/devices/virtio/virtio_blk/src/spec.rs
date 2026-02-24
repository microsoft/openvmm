// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtio block device specification constants and types.

use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Virtio block device ID.
pub const VIRTIO_BLK_DEVICE_ID: u16 = 2;

// Feature bits (bits in bank 0, device-specific bits 0..23)
pub const VIRTIO_BLK_F_SIZE_MAX: u32 = 1 << 1;
pub const VIRTIO_BLK_F_SEG_MAX: u32 = 1 << 2;
pub const VIRTIO_BLK_F_RO: u32 = 1 << 5;
pub const VIRTIO_BLK_F_BLK_SIZE: u32 = 1 << 6;
pub const VIRTIO_BLK_F_FLUSH: u32 = 1 << 9;
pub const VIRTIO_BLK_F_TOPOLOGY: u32 = 1 << 10;
pub const VIRTIO_BLK_F_DISCARD: u32 = 1 << 13;
pub const VIRTIO_BLK_F_WRITE_ZEROES: u32 = 1 << 14;

// Request types
pub const VIRTIO_BLK_T_IN: u32 = 0;
pub const VIRTIO_BLK_T_OUT: u32 = 1;
pub const VIRTIO_BLK_T_FLUSH: u32 = 4;
pub const VIRTIO_BLK_T_GET_ID: u32 = 8;
pub const VIRTIO_BLK_T_DISCARD: u32 = 11;
pub const VIRTIO_BLK_T_WRITE_ZEROES: u32 = 13;

// Status codes
pub const VIRTIO_BLK_S_OK: u8 = 0;
pub const VIRTIO_BLK_S_IOERR: u8 = 1;
pub const VIRTIO_BLK_S_UNSUPP: u8 = 2;

/// Maximum length of the device ID string.
pub const VIRTIO_BLK_ID_BYTES: usize = 20;

/// Maximum segment size we advertise.
pub const DEFAULT_SIZE_MAX: u32 = 1 << 22; // 4MB
/// Maximum number of segments per request.
pub const DEFAULT_SEG_MAX: u32 = 128;

/// Virtio block device config space layout.
/// Fields are always little-endian.
#[repr(C)]
#[derive(Debug, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VirtioBlkConfig {
    /// Capacity in 512-byte sectors.
    pub capacity: u64,
    /// Maximum segment size (if VIRTIO_BLK_F_SIZE_MAX).
    pub size_max: u32,
    /// Maximum number of segments (if VIRTIO_BLK_F_SEG_MAX).
    pub seg_max: u32,
    /// Geometry (if VIRTIO_BLK_F_GEOMETRY).
    pub geometry: VirtioBlkGeometry,
    /// Block size in bytes (if VIRTIO_BLK_F_BLK_SIZE).
    pub blk_size: u32,
    /// Topology (if VIRTIO_BLK_F_TOPOLOGY).
    pub topology: VirtioBlkTopology,
    /// Writeback mode (if VIRTIO_BLK_F_CONFIG_WCE).
    pub writeback: u8,
    pub unused0: u8,
    /// Number of queues (if VIRTIO_BLK_F_MQ).
    pub num_queues: u16,
    /// Maximum discard sectors (if VIRTIO_BLK_F_DISCARD).
    pub max_discard_sectors: u32,
    /// Maximum discard segments (if VIRTIO_BLK_F_DISCARD).
    pub max_discard_seg: u32,
    /// Discard sector alignment (if VIRTIO_BLK_F_DISCARD).
    pub discard_sector_alignment: u32,
    /// Maximum write zeroes sectors (if VIRTIO_BLK_F_WRITE_ZEROES).
    pub max_write_zeroes_sectors: u32,
    /// Maximum write zeroes segments (if VIRTIO_BLK_F_WRITE_ZEROES).
    pub max_write_zeroes_seg: u32,
    /// Whether write zeroes may unmap (if VIRTIO_BLK_F_WRITE_ZEROES).
    pub write_zeroes_may_unmap: u8,
    pub unused1: [u8; 3],
    // Explicit padding to satisfy alignment requirements for IntoBytes.
    // Not part of the virtio config space (device_register_length excludes this).
    pub _padding: [u8; 4],
}

#[repr(C)]
#[derive(Debug, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VirtioBlkGeometry {
    pub cylinders: u16,
    pub heads: u8,
    pub sectors: u8,
}

#[repr(C)]
#[derive(Debug, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
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
#[derive(Debug, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VirtioBlkReqHeader {
    pub request_type: u32,
    pub reserved: u32,
    pub sector: u64,
}

/// Discard/write zeroes data segment.
#[repr(C)]
#[derive(Debug, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VirtioBlkDiscardWriteZeroes {
    pub sector: u64,
    pub num_sectors: u32,
    pub flags: u32,
}
