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
pub struct SratHeader {
    pub rsvd1: u32_ne,
    pub rsvd2: u64_ne,
}

impl SratHeader {
    pub fn new() -> SratHeader {
        SratHeader {
            rsvd1: 1.into(),
            rsvd2: 0.into(),
        }
    }
}

impl Table for SratHeader {
    const SIGNATURE: [u8; 4] = *b"SRAT";
}

pub const SRAT_REVISION: u8 = 3;

open_enum::open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
    pub enum SratType: u8 {
        APIC = 0,
        MEMORY = 1,
        X2APIC = 2,
        GICC = 3,
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct SratApic {
    pub typ: SratType,
    pub length: u8,
    pub proximity_domain_byte1: u8,
    pub apic_id: u8,
    pub flags: u32_ne,
    pub local_sapic_eid: u8,
    pub proximity_domain_byte2: u8,
    pub proximity_domain_byte3: u8,
    pub proximity_domain_byte4: u8,
    pub clock_domain: u32_ne,
}

const_assert_eq!(size_of::<SratApic>(), 16);

pub const SRAT_APIC_ENABLED: u32 = 1 << 0;

impl SratApic {
    pub fn new(apic_id: u8, vnode: u32) -> Self {
        let vnode = vnode.to_le_bytes();
        Self {
            typ: SratType::APIC,
            length: size_of::<Self>() as u8,
            proximity_domain_byte1: vnode[0],
            apic_id,
            flags: SRAT_APIC_ENABLED.into(),
            local_sapic_eid: 0,
            proximity_domain_byte2: vnode[1],
            proximity_domain_byte3: vnode[2],
            proximity_domain_byte4: vnode[3],
            clock_domain: 0.into(),
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct SratX2Apic {
    pub typ: SratType,
    pub length: u8,
    pub reserved: u16_ne,
    pub proximity_domain: u32_ne,
    pub x2_apic_id: u32_ne,
    pub flags: u32_ne,
    pub clock_domain: u32_ne,
    pub reserved2: u32_ne,
}

const_assert_eq!(size_of::<SratX2Apic>(), 24);

impl SratX2Apic {
    pub fn new(x2_apic_id: u32, vnode: u32) -> Self {
        Self {
            typ: SratType::X2APIC,
            length: size_of::<Self>() as u8,
            x2_apic_id: x2_apic_id.into(),
            flags: SRAT_APIC_ENABLED.into(),
            clock_domain: 0.into(),
            reserved: 0.into(),
            proximity_domain: vnode.into(),
            reserved2: 0.into(),
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct SratGicc {
    pub typ: SratType,
    pub length: u8,
    pub proximity_domain: u32_ne,
    pub acpi_processor_uid: u32_ne,
    pub flags: u32_ne,
    pub clock_domain: u32_ne,
}

const_assert_eq!(size_of::<SratGicc>(), 18);

impl SratGicc {
    pub fn new(acpi_processor_uid: u32, vnode: u32) -> Self {
        Self {
            typ: SratType::GICC,
            length: size_of::<Self>() as u8,
            acpi_processor_uid: acpi_processor_uid.into(),
            flags: SRAT_APIC_ENABLED.into(),
            clock_domain: 0.into(),
            proximity_domain: vnode.into(),
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct SratMemory {
    pub typ: SratType,
    pub length: u8,
    pub proximity_domain: u32_ne,
    pub rsvd1: u16_ne,
    pub low_address: u32_ne,
    pub high_address: u32_ne,
    pub low_length: u32_ne,
    pub high_length: u32_ne,
    pub rsvd2: u32_ne,
    pub flags: u32_ne,
    pub rsvd3: u64_ne,
}

impl core::fmt::Debug for SratMemory {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let address =
            u64::read_from_bytes([self.low_address, self.high_address].as_bytes()).unwrap();
        let length = u64::read_from_bytes([self.low_length, self.high_length].as_bytes()).unwrap();

        f.debug_struct("SratMemory")
            .field("typ", &self.typ)
            .field("length", &self.length)
            .field("proximity_domain", &self.proximity_domain)
            .field("rsvd1", &self.rsvd1)
            .field("address", &address)
            .field("_end_address", &(address + length))
            .field("length", &length)
            .field("rsvd2", &self.rsvd2)
            .field("flags", &self.flags)
            .field("rsvd3", &self.rsvd3)
            .finish()
    }
}

const_assert_eq!(size_of::<SratMemory>(), 40);

open_enum::open_enum! {
    pub enum SratMemoryFlags: u32 {
        ENABLED       = 1 << 0,
        HOT_PLUGGABLE = 1 << 1,
        NVRAM         = 1 << 2,
    }
}

impl SratMemory {
    pub fn new(addr: u64, len: u64, vnode: u32) -> Self {
        Self {
            typ: SratType::MEMORY,
            length: size_of::<Self>() as u8,
            proximity_domain: vnode.into(),
            rsvd1: 0.into(),
            low_address: (addr as u32).into(),
            high_address: ((addr >> 32) as u32).into(),
            low_length: (len as u32).into(),
            high_length: ((len >> 32) as u32).into(),
            rsvd2: 0.into(),
            flags: SratMemoryFlags::ENABLED.0.into(),
            rsvd3: 0.into(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ParseSratError {
    #[error("could not read standard ACPI header")]
    MissingAcpiHeader,
    #[error("invalid signature. expected b\"SRAT\", found {0:?}")]
    InvalidSignature([u8; 4]),
    #[error("mismatched len. in_header: {in_header}, actual {actual}")]
    MismatchedLength { in_header: usize, actual: usize },
    #[error("missing fixed SRAT header")]
    MissingFixedHeader,
    #[error("could not read APIC structure")]
    BadApic,
    #[error("could not read MEMORY structure")]
    BadMemory,
    #[error("unknown SRAT structure type: {0}")]
    UnknownType(u8),
}

pub fn parse_srat<'a>(
    raw_srat: &'a [u8],
    mut on_apic: impl FnMut(&'a SratApic),
    mut on_memory: impl FnMut(&'a SratMemory),
) -> Result<(&'a crate::Header, &'a SratHeader), ParseSratError> {
    let raw_srat_len = raw_srat.len();
    let (acpi_header, buf) = Ref::<_, crate::Header>::from_prefix(raw_srat)
        .map_err(|_| ParseSratError::MissingAcpiHeader)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

    if acpi_header.signature != *b"SRAT" {
        return Err(ParseSratError::InvalidSignature(acpi_header.signature));
    }

    if acpi_header.length.get() as usize != raw_srat_len {
        return Err(ParseSratError::MismatchedLength {
            in_header: acpi_header.length.get() as usize,
            actual: raw_srat_len,
        });
    }

    let (srat_header, mut buf) =
        Ref::<_, SratHeader>::from_prefix(buf).map_err(|_| ParseSratError::MissingFixedHeader)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

    while !buf.is_empty() {
        buf = match SratType(buf[0]) {
            SratType::APIC => {
                let (apic, rest) =
                    Ref::<_, SratApic>::from_prefix(buf).map_err(|_| ParseSratError::BadApic)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                on_apic(Ref::into_ref(apic));
                rest
            }
            SratType::MEMORY => {
                let (mem, rest) = Ref::<_, SratMemory>::from_prefix(buf)
                    .map_err(|_| ParseSratError::BadMemory)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                on_memory(Ref::into_ref(mem));
                rest
            }
            _ => return Err(ParseSratError::UnknownType(buf[0])),
        }
    }

    Ok((Ref::into_ref(acpi_header), Ref::into_ref(srat_header)))
}

#[cfg(feature = "alloc")]
pub mod alloc_parse {
    use super::*;
    use alloc::vec::Vec;

    #[derive(Debug)]
    pub struct BorrowedSrat<'a> {
        pub acpi_header: &'a crate::Header,
        pub srat_header: &'a SratHeader,
        pub apics: Vec<&'a SratApic>,
        pub memory: Vec<&'a SratMemory>,
    }

    #[derive(Debug)]
    pub struct OwnedSrat {
        pub acpi_header: crate::Header,
        pub srat_header: SratHeader,
        pub apics: Vec<SratApic>,
        pub memory: Vec<SratMemory>,
    }

    impl From<BorrowedSrat<'_>> for OwnedSrat {
        fn from(b: BorrowedSrat<'_>) -> Self {
            OwnedSrat {
                acpi_header: *b.acpi_header,
                srat_header: *b.srat_header,
                apics: b.apics.into_iter().cloned().collect(),
                memory: b.memory.into_iter().cloned().collect(),
            }
        }
    }

    impl BorrowedSrat<'_> {
        pub fn new(raw_srat: &[u8]) -> Result<BorrowedSrat<'_>, ParseSratError> {
            let mut apics = Vec::new();
            let mut memory = Vec::new();
            let (acpi_header, srat_header) =
                parse_srat(raw_srat, |x| apics.push(x), |x| memory.push(x))?;

            Ok(BorrowedSrat {
                acpi_header,
                srat_header,
                apics,
                memory,
            })
        }
    }

    impl OwnedSrat {
        pub fn new(raw_srat: &[u8]) -> Result<OwnedSrat, ParseSratError> {
            Ok(BorrowedSrat::new(raw_srat)?.into())
        }
    }
}
