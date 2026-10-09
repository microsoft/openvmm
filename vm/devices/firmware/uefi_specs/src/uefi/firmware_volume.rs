// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PI firmware volume, file, and section definitions.

use guid::Guid;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::KnownLayout;

/// PI firmware volume header signature.
pub const FV_SIGNATURE: u32 = u32::from_le_bytes(*b"_FVH");

/// PI firmware section type for raw data.
pub const SECTION_RAW: u8 = 0x19;
/// PI firmware section type for a PE32 image.
pub const SECTION_PE32: u8 = 0x10;
/// PI firmware file type for the security core.
pub const FILETYPE_SECURITY_CORE: u8 = 0x03;

#[repr(C)]
#[derive(FromBytes, Immutable, KnownLayout)]
pub struct FirmwareVolumeHeader {
    pub zero_vector: [u8; 16],
    pub file_system_guid: Guid,
    pub fv_length: u64,
    pub signature: u32,
    pub attributes: u32,
    pub header_length: u16,
    pub checksum: u16,
    pub ext_header_offset: u16,
    pub reserved: u8,
    pub revision: u8,
}

#[repr(C)]
#[derive(FromBytes, Immutable, KnownLayout)]
pub struct FfsFileHeader {
    pub name: Guid,
    pub integrity_check: u16,
    pub file_type: u8,
    pub attributes: u8,
    pub size: [u8; 3],
    pub state: u8,
}

#[repr(C)]
#[derive(FromBytes, Immutable, KnownLayout)]
pub struct CommonSectionHeader {
    pub size: [u8; 3],
    pub section_type: u8,
}
