// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::packed_nums::u16_ne;
use crate::packed_nums::u32_ne;
use crate::Table;
use bitfield_struct::bitfield;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::Unaligned;

/// PPTT table, used for describing the cache topology of a machine.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct Pptt {}

impl Table for Pptt {
    const SIGNATURE: [u8; 4] = *b"PPTT";
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
    pub enum PpttType: u8 {
        PROCESSOR = 0,
        CACHE = 1,
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct PpttProcessor {
    pub typ: PpttType,
    pub len: u8,
    pub rsvd: [u8; 2],
    pub flags: u32_ne,
    pub parent: u32_ne,
    pub acpi_processor_id: u32_ne,
    pub num_private_resources: u32_ne,
}

#[bitfield(u32)]
pub struct PpttProcessorFlags {
    pub physical_package: bool,
    pub acpi_processor_uid_valid: bool,
    pub processor_is_a_thread: bool,
    pub node_is_a_leaf: bool,
    pub identical_implementation: bool,
    #[bits(27)]
    _rsvd: u32,
}

const _: () = assert!(size_of::<PpttProcessor>() == 20);

impl PpttProcessor {
    pub fn new(num_private_resources: u8) -> Self {
        Self {
            typ: PpttType::PROCESSOR,
            len: size_of::<Self>() as u8 + num_private_resources * 4,
            num_private_resources: (num_private_resources as u32).into(),
            ..FromZeros::new_zeroed()
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct PpttCache {
    pub typ: PpttType,
    pub len: u8,
    pub rsvd: [u8; 2],
    pub flags: u32_ne,
    pub next_level: u32_ne,
    pub size: u32_ne,
    pub num_sets: u32_ne,
    pub associativity: u8,
    pub attributes: PpttCacheAttributes,
    pub line_size: u16_ne,
    pub cache_id: u32_ne,
}

const _: () = assert!(size_of::<PpttCache>() == 28);

impl PpttCache {
    pub fn new() -> Self {
        Self {
            typ: PpttType::CACHE,
            len: size_of::<Self>() as u8,
            ..FromZeros::new_zeroed()
        }
    }
}

#[bitfield(u32)]
pub struct PpttCacheFlags {
    pub size_valid: bool,
    pub number_of_sets_valid: bool,
    pub associativity_valid: bool,
    pub allocation_type_valid: bool,
    pub cache_type_valid: bool,
    pub write_policy_valid: bool,
    pub line_size_valid: bool,
    pub cache_id_valid: bool,
    #[bits(24)]
    _rsvd: u32,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct PpttCacheAttributes {
    #[bits(2)]
    pub allocation_type: u8,
    #[bits(2)]
    pub cache_type: u8,
    pub write_through: bool,
    #[bits(3)]
    _rsvd: u8,
}

pub const PPTT_CACHE_TYPE_DATA: u8 = 0;
pub const PPTT_CACHE_TYPE_INSTRUCTION: u8 = 1;
pub const PPTT_CACHE_TYPE_UNIFIED: u8 = 3;
