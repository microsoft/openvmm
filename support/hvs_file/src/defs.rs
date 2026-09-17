// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! On-disk structure definitions for the HyperV Storage file format.
//!
//! These types represent the binary layout of `.vmrs`, `.vmcx`, and `.vsv`
//! files. All multi-byte integers are little-endian. Structures are packed.

#![expect(dead_code)]

use core::mem::size_of;
use open_enum::open_enum;
use static_assertions::const_assert_eq;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

// ============================================================
// File Header
// ============================================================

/// File header signature.
pub const HEADER_SIGNATURE: u32 = 0x0128_2014;

/// Object table signature.
pub const OBJECT_TABLE_SIGNATURE: u32 = 0x0111_0001;

/// Key table signature.
pub const KEY_TABLE_SIGNATURE: u16 = 0x0002;

/// Default data alignment (one page).
pub const DEFAULT_DATA_ALIGNMENT: u32 = 4096;

/// Minimum data alignment (also the header copy size).
pub const MIN_DATA_ALIGNMENT: u32 = 4096;

/// Default key table size.
pub const DEFAULT_KEY_TABLE_SIZE: u32 = 4096;

/// Threshold above which values are stored as file objects.
pub const FILE_OBJECT_THRESHOLD: u32 = 2048;

/// Format version 4.0 (current).
pub const FORMAT_VERSION_4_0: u32 = 0x0400;

/// File header — stored twice at offsets 0 and 4096, padded to
/// `data_alignment_in_bytes`.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FileHeader {
    pub signature: u32,
    pub checksum: u32,
    pub sequence: u16,
    pub format_version: u32,
    pub data_version: u32,
    pub flags: u32,
    pub data_alignment_in_bytes: u32,
    pub replay_log_offset_in_bytes: u64,
    pub replay_log_size_in_bytes: u64,
    pub replay_log_header_size_in_bytes: u32,
}

const_assert_eq!(size_of::<FileHeader>(), 46);

// ============================================================
// Object Table
// ============================================================

/// Object table header.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ObjectTableHeader {
    pub signature: u32,
    pub entries_count: u32,
}

const_assert_eq!(size_of::<ObjectTableHeader>(), 8);

open_enum! {
    /// Object type in an object table entry.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum ObjectType: u8 {
        EMPTY       = 0,
        OBJECT_TABLE = 1,
        KEY_TABLE   = 2,
        FILE_OBJECT = 3,
        FREE        = 4,
    }
}

/// Object table entry.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ObjectTableEntry {
    pub object_type: ObjectType,
    pub entry_checksum: u32,
    pub file_offset_in_bytes: u64,
    pub size_in_bytes: u32,
    pub flags: u8,
}

const_assert_eq!(size_of::<ObjectTableEntry>(), 18);

/// Flag indicating the object is required.
pub const OBJECT_ENTRY_FLAG_REQUIRED: u8 = 0x01;

// ============================================================
// Key Table
// ============================================================

/// Key table header. Checksum covers only this header with the
/// checksum field zeroed.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct KeyTableHeader {
    pub signature: u16,
    pub table_index: u16,
    pub sequence: u16,
    pub checksum: u32,
}

const_assert_eq!(size_of::<KeyTableHeader>(), 10);

open_enum! {
    /// Key entry type.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum KeyType: u8 {
        FREE    = 1,
        INT     = 3,
        UINT    = 4,
        DOUBLE  = 5,
        STRING  = 6,
        ARRAY   = 7,
        BOOL    = 8,
        NODE    = 9,
    }
}

/// Key table entry header. Checksum covers the header (with checksum
/// field zeroed) plus the name and data bytes.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct KeyTableEntryHeader {
    pub key_type: KeyType,
    pub flags: u8,
    pub size_in_bytes: u32,
    pub parent_node_table: u16,
    pub parent_node_offset: u32,
    pub checksum: u32,
    pub insertion_sequence: u32,
    pub name_size_in_symbols: u8,
}

const_assert_eq!(size_of::<KeyTableEntryHeader>(), 21);

/// Key entry flag: data is a file object pointer, not inline.
pub const KEY_FLAG_POINTS_TO_FILE_OBJECT: u8 = 0x01;
/// Key entry flag: entry is a subcomponent.
pub const KEY_FLAG_SUBCOMPONENT: u8 = 0x02;
/// Key entry flag: sequence change tracking enabled.
pub const KEY_FLAG_SEQUENCE_CHANGE_TRACKING: u8 = 0x04;

/// Node data stored inline for `KeyType::NODE` entries.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct NodeData {
    pub change_tracking_sequence: u64,
    pub next_insertion_sequence: u32,
}

const_assert_eq!(size_of::<NodeData>(), 12);

/// File object data pointer, stored in lieu of inline data when the
/// `KEY_FLAG_POINTS_TO_FILE_OBJECT` flag is set.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FileObjectData {
    pub size_in_bytes: u32,
    pub offset_in_bytes: u64,
}

const_assert_eq!(size_of::<FileObjectData>(), 12);

// ============================================================
// Replay Log
// ============================================================

/// Replay log signature.
pub const REPLAY_LOG_SIGNATURE: u32 = 0x0111_0003;

/// Replay log header.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReplayLogHeader {
    pub signature: u32,
    pub checksum: u32,
    pub current_entries_count: u32,
    pub reserved: u8,
    pub maximum_number_of_entries: u32,
    pub change_tracking_enabled: u8,
    pub change_tracking_buffer_offset: u64,
    pub change_tracking_buffer_size: u32,
    pub change_tracking_buffer_used_size: u32,
}

const_assert_eq!(size_of::<ReplayLogHeader>(), 34);

/// Replay log entry header.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ReplayLogEntryHeader {
    pub header_checksum: u32,
    pub destination_offset: u64,
    pub destination_size: u32,
    pub offset_inside_log: u64,
    pub data_checksum: u32,
}

const_assert_eq!(size_of::<ReplayLogEntryHeader>(), 28);
