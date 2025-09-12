// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(feature = "alloc")]
pub use self::alloc_parse::*;

use super::Table;
use crate::packed_nums::*;
use core::mem::size_of;
use static_assertions::const_assert_eq;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::Ref;
use zerocopy::Unaligned;

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct McfgHeader {
    pub rsvd: u64_ne,
}

impl McfgHeader {
    pub fn new() -> Self {
        McfgHeader { rsvd: 0.into() }
    }
}

impl Table for McfgHeader {
    const SIGNATURE: [u8; 4] = *b"MCFG";
}

pub const MCFG_REVISION: u8 = 1;

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct McfgSegmentBusRange {
    pub ecam_base: u64_ne,
    pub segment: u16_ne,
    pub start_bus: u8,
    pub end_bus: u8,
    pub rsvd: u32_ne,
}

const_assert_eq!(size_of::<McfgSegmentBusRange>(), 16);

impl McfgSegmentBusRange {
    pub fn new(ecam_base: u64, segment: u16, start_bus: u8, end_bus: u8) -> Self {
        Self {
            ecam_base: ecam_base.into(),
            segment: segment.into(),
            start_bus,
            end_bus,
            rsvd: 0.into(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ParseMcfgError {
    #[error("could not read standard ACPI header")]
    MissingAcpiHeader,
    #[error("invalid signature. expected b\"MCFG\", found {0:?}")]
    InvalidSignature([u8; 4]),
    #[error("mismatched lengh, header: {0}, actual: {1}")]
    MismatchedLength(usize, usize),
    #[error("could not read fixed MCFG header")]
    MissingFixedHeader,
    #[error("could not read segment bus range structure")]
    BadSegmentBusRange,
}

pub fn parse_mcfg<'a>(
    raw_mcfg: &'a [u8],
    mut on_segment_bus_range: impl FnMut(&'a McfgSegmentBusRange),
) -> Result<(&'a crate::Header, &'a McfgHeader), ParseMcfgError> {
    let raw_mcfg_len = raw_mcfg.len();
    let (acpi_header, buf) = Ref::<_, crate::Header>::from_prefix(raw_mcfg)
        .map_err(|_| ParseMcfgError::MissingAcpiHeader)?;

    if acpi_header.signature != *b"MCFG" {
        return Err(ParseMcfgError::InvalidSignature(acpi_header.signature));
    }

    if acpi_header.length.get() as usize != raw_mcfg_len {
        return Err(ParseMcfgError::MismatchedLength(
            acpi_header.length.get() as usize,
            raw_mcfg_len,
        ));
    }

    let (mcfg_header, mut buf) =
        Ref::<_, McfgHeader>::from_prefix(buf).map_err(|_| ParseMcfgError::MissingFixedHeader)?;

    while !buf.is_empty() {
        let (sbr, rest) = Ref::<_, McfgSegmentBusRange>::from_prefix(buf)
            .map_err(|_| ParseMcfgError::BadSegmentBusRange)?;
        on_segment_bus_range(Ref::into_ref(sbr));
        buf = rest
    }

    Ok((Ref::into_ref(acpi_header), Ref::into_ref(mcfg_header)))
}

#[cfg(feature = "alloc")]
pub mod alloc_parse {
    use super::*;
    use alloc::vec::Vec;

    #[derive(Debug)]
    pub struct BorrowedMcfg<'a> {
        pub acpi_header: &'a crate::Header,
        pub mcfg_header: &'a McfgHeader,
        pub segment_bus_ranges: Vec<&'a McfgSegmentBusRange>,
    }

    #[derive(Debug)]
    pub struct OwnedMcfg {
        pub acpi_header: crate::Header,
        pub mcfg_header: McfgHeader,
        pub segment_bus_ranges: Vec<McfgSegmentBusRange>,
    }

    impl From<BorrowedMcfg<'_>> for OwnedMcfg {
        fn from(b: BorrowedMcfg<'_>) -> Self {
            OwnedMcfg {
                acpi_header: *b.acpi_header,
                mcfg_header: *b.mcfg_header,
                segment_bus_ranges: b.segment_bus_ranges.into_iter().copied().collect(),
            }
        }
    }

    impl BorrowedMcfg<'_> {
        pub fn new(raw_mcfg: &[u8]) -> Result<BorrowedMcfg<'_>, ParseMcfgError> {
            let mut segment_bus_ranges = Vec::new();
            let (acpi_header, mcfg_header) = parse_mcfg(raw_mcfg, |x| segment_bus_ranges.push(x))?;

            Ok(BorrowedMcfg {
                acpi_header,
                mcfg_header,
                segment_bus_ranges,
            })
        }
    }

    impl OwnedMcfg {
        pub fn new(raw_mcfg: &[u8]) -> Result<OwnedMcfg, ParseMcfgError> {
            Ok(BorrowedMcfg::new(raw_mcfg)?.into())
        }
    }
}
