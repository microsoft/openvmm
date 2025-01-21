// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![allow(dead_code)]

use zerocopy::BigEndian;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

pub type U32b = zerocopy::U32<BigEndian>;
pub type U64b = zerocopy::U64<BigEndian>;

/// The header for the overall FDT.
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Header {
    pub magic: U32b,
    pub totalsize: U32b,
    pub off_dt_struct: U32b,
    pub off_dt_strings: U32b,
    pub off_mem_rsvmap: U32b,
    pub version: U32b,
    pub last_comp_version: U32b,
    pub boot_cpuid_phys: U32b,
    pub size_dt_strings: U32b,
    pub size_dt_struct: U32b,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq, Clone, Copy)]
/// A single entry in the memory reservation map, `/memreserve/`.
pub struct ReserveEntry {
    /// The address of the reserved memory.
    pub address: U64b,
    /// The size of the reserved memory.
    pub size: U64b,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PropHeader {
    pub len: U32b,
    pub nameoff: U32b,
}

pub const BEGIN_NODE: u32 = 1;
pub const END_NODE: u32 = 2;
pub const PROP: u32 = 3;
pub const NOP: u32 = 4;
pub const END: u32 = 9;

pub const MAGIC: u32 = 0xd00dfeed;

pub const CURRENT_VERSION: u32 = 17;
pub const COMPAT_VERSION: u32 = 16;
