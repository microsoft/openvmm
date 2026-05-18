// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! On-disk structure definitions for VMRS saved state files.
//!
//! These are worker process structures from `onecore/vm/worker/` — not
//! hypervisor definitions. They describe the memory layout metadata
//! stored in VMRS key-value entries.

use static_assertions::const_assert_eq;
use std::mem::size_of;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Version constant for the memory block save state struct.
pub const WPMM_MB_SAVE_STATE_VERSION_3: u32 = 3;

/// Memory block metadata (`MEMORY_BLOCK_OBJECT_SAVE_STRUCT_CURRENT`).
///
/// From `onecore/vm/worker/inc/MemoryBlockObjectSaveStruct.h`. Stored as the
/// `RamMemoryBlock%d` array values in the VMRS key-value store. Each instance
/// describes a contiguous guest physical memory range and maps it to the
/// corresponding `RamBlock%I64u` data keys.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MemoryBlockSaveStruct {
    /// Must be [`WPMM_MB_SAVE_STATE_VERSION_3`] (3).
    pub saved_state_version: u32,
    /// Flags (IsHotAdded, IsSgx, IsVtl2Mb, IsSpecificPurpose). Zero for dumps.
    pub flags: u32,
    /// Number of 4K pages in this memory block.
    pub page_count_total: u64,
    /// Starting MBP (memory block page) index into the data block sequence.
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
