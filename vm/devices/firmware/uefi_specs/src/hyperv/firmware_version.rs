// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MSVM firmware build and interface version record.

use guid::Guid;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::KnownLayout;

/// GUID of the FFS file containing the MSVM firmware version record.
pub const FILE_GUID: Guid = guid::guid!("b8f2d3a4-6c7e-4e1b-9a2f-1d3c5e7a9b0d");

pub const SIGNATURE: &[u8; 4] = b"MVFW";
pub const STRUCT_VERSION: u16 = 1;
pub const HEADER_SIZE: usize = 80;

/// The firmware was built from a dirty source tree.
pub const FLAG_DIRTY: u32 = 1;
/// The firmware was produced by the official build pipeline.
pub const FLAG_OFFICIAL: u32 = 2;

/// Packed `MSVM_FIRMWARE_VERSION_INFO` record.
#[repr(C)]
#[derive(Debug, FromBytes, Immutable, KnownLayout)]
pub struct MsvmFirmwareVersionInfo {
    pub signature: [u8; 4],
    pub struct_version: [u8; 2],
    pub header_size: [u8; 2],
    pub flags: [u8; 4],
    pub interface_version_major: [u8; 2],
    pub interface_version_minor: [u8; 2],
    pub base_version: [u8; 16],
    pub git_commit: [u8; 48],
}

static_assertions::const_assert_eq!(size_of::<MsvmFirmwareVersionInfo>(), HEADER_SIZE);
static_assertions::const_assert_eq!(align_of::<MsvmFirmwareVersionInfo>(), 1);
