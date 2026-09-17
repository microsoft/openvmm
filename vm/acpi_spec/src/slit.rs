// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Table;
use crate::packed_nums::*;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::Unaligned;

/// SLIT table header (after the standard ACPI header).
///
/// The SLIT body is an N×N matrix of `u8` distances appended after this header.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct SlitHeader {
    pub number_of_system_localities: u64_ne,
}

impl SlitHeader {
    pub fn new(num_localities: u64) -> Self {
        Self {
            number_of_system_localities: num_localities.into(),
        }
    }
}

impl Table for SlitHeader {
    const SIGNATURE: [u8; 4] = *b"SLIT";
}

pub const SLIT_REVISION: u8 = 1;
