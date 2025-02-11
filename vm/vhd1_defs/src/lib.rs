// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VHD1 file format definitions.
//!
//! Currently incomplete (missing defs for non-fixed disks).

#![no_std]

use self::packed_nums::*;
use guid::Guid;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

#[allow(non_camel_case_types)]
mod packed_nums {
    pub type u32_be = zerocopy::U32<zerocopy::BigEndian>;
    pub type u64_be = zerocopy::U64<zerocopy::BigEndian>;
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VhdFooter {
    pub cookie: u64_be,
    pub features: u32_be,
    pub file_format_version: u32_be,
    pub data_offset: u64_be,
    pub time_stamp: u32_be,
    pub creator_application: u32_be,
    pub creator_version: u32_be,
    pub creator_host_os: u32_be,
    pub original_size: u64_be,
    pub current_size: u64_be,
    pub disk_geometry: u32_be,
    pub disk_type: u32_be,
    pub checksum: u32_be,
    pub unique_id: Guid,
    pub saved_state: u8,
    pub reserved: [u8; 427],
}

impl VhdFooter {
    pub const LEN: u64 = 512;
    pub const ALIGNMENT: u64 = 512;

    pub const COOKIE_MAGIC: u64_be = u64_be::from_bytes(*b"conectix");
    pub const FEATURE_MASK: u32 = 0x2;
    pub const FILE_FORMAT_VERSION_MAGIC: u32 = 0x00010000;
    pub const FIXED_DATA_OFFSET: u64 = !0;
    pub const CREATOR_VERSION_MAGIC: u32 = 0x000a0000;
    pub const DISK_TYPE_FIXED: u32 = 2;

    pub fn new_fixed(size: u64, guid: Guid) -> Self {
        let mut footer = Self {
            cookie: Self::COOKIE_MAGIC,
            features: Self::FEATURE_MASK.into(),
            file_format_version: Self::FILE_FORMAT_VERSION_MAGIC.into(),
            data_offset: Self::FIXED_DATA_OFFSET.into(),
            creator_version: Self::CREATOR_VERSION_MAGIC.into(),
            original_size: size.into(),
            current_size: size.into(),
            disk_type: Self::DISK_TYPE_FIXED.into(),
            ..FromZeros::new_zeroed()
        };

        footer.unique_id = guid;
        footer.checksum = footer.compute_checksum().into();
        footer
    }

    pub fn compute_checksum(&self) -> u32 {
        !(self.as_bytes().iter().map(|b| *b as u32).sum::<u32>()
            - self
                .checksum
                .as_bytes()
                .iter()
                .map(|b| *b as u32)
                .sum::<u32>())
    }
}
