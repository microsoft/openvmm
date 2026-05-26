// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Table;
use core::mem::size_of;
use static_assertions::const_assert_eq;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::Unaligned;

/// ACPI 6.5 Generic Timer Description Table (Table 5-128).
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, Default, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct Gtdt {
    pub cnt_control_base: u64,
    pub reserved: u32,
    pub secure_el1_timer_gsiv: u32,
    pub secure_el1_timer_flags: u32,
    pub non_secure_el1_timer_gsiv: u32,
    pub non_secure_el1_timer_flags: u32,
    pub virtual_el1_timer_gsiv: u32,
    pub virtual_el1_timer_flags: u32,
    pub el2_timer_gsiv: u32,
    pub el2_timer_flags: u32,
    pub cnt_read_base: u64,
    pub platform_timer_count: u32,
    pub platform_timer_offset: u32,
    pub virtual_el2_timer_gsiv: u32,
    pub virtual_el2_timer_flags: u32,
}

const_assert_eq!(size_of::<Gtdt>(), 68);

impl Table for Gtdt {
    const SIGNATURE: [u8; 4] = *b"GTDT";
}

pub const GTDT_TIMER_EDGE_TRIGGERED: u32 = 1 << 0;
pub const GTDT_TIMER_ACTIVE_LOW: u32 = 1 << 1;
pub const GTDT_TIMER_ALWAYS_ON: u32 = 1 << 2;
