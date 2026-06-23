// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! On-disk structure definitions for VMRS saved state files.

use static_assertions::const_assert_eq;
use std::mem::size_of;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

// ============================================================
// VID Saved State Descriptor
// ============================================================

/// Envelope wrapping the partition state chunk stream.
///
/// The chunk data starts at offset `header_size + 16` from the start of
/// the blob. The first 16 bytes after `header_size` are skipped for
/// alignment.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VidSavedStateDescriptor {
    /// Size of the descriptor + any pre-data area.
    pub descriptor_size: u64,
    /// Size of the pre-data sections (descriptor + header areas).
    pub header_size: u64,
    /// Total blob size.
    pub total_size: u64,
}

// ============================================================
// Memory Block Definitions
// ============================================================

/// Version constant for the memory block save state struct.
pub const MEMORY_BLOCK_SAVE_VERSION: u32 = 3;

/// Memory block metadata.
///
/// Stored as the `RamMemoryBlock%d` array values in the VMRS key-value
/// store. Each instance describes a contiguous guest physical memory
/// range and maps it to the corresponding `RamBlock%I64u` data keys.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MemoryBlockSaveStruct {
    /// Must be [`MEMORY_BLOCK_SAVE_VERSION`] (3).
    pub saved_state_version: u32,
    /// Flags. Zero for dumps.
    pub flags: u32,
    /// Number of 4K pages in this memory block.
    pub page_count_total: u64,
    /// Starting memory block page index into the data block sequence.
    pub mbp_index_start: u64,
    /// Starting GPA page number (GPA byte address / 4096).
    pub gpa_index_start: u64,
    /// NUMA node index.
    pub virtual_node: u32,
    pub _padding: u32,
    /// KSR block ID (zero for debug dumps).
    pub ksr_block_id: u64,
}

const_assert_eq!(size_of::<MemoryBlockSaveStruct>(), 48);

/// VM version for dump files (v10.0 = 0x0A00).
pub const VM_VERSION: i64 = 0x0A00;

/// Size of one guest memory data block in bytes (1 MiB).
pub const DATA_BLOCK_SIZE: usize = 1_048_576;

/// Size of one guest memory data block in 4K pages (256).
pub const DATA_BLOCK_PAGES: u64 = (DATA_BLOCK_SIZE / 4096) as u64;
