// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! IORT (IO Remapping Table) types for aarch64 PCIe topology.

use super::Table;
use crate::packed_nums::*;
use core::mem::size_of;
use static_assertions::const_assert_eq;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::Unaligned;

pub const IORT_REVISION: u8 = 5;
pub const IORT_NODE_OFFSET: u32 = size_of::<crate::Header>() as u32 + size_of::<Iort>() as u32;

pub const IORT_NODE_TYPE_ITS_GROUP: u8 = 0x00;
pub const IORT_NODE_TYPE_PCI_ROOT_COMPLEX: u8 = 0x02;

pub const IORT_PCI_ROOT_COMPLEX_REVISION: u8 = 3;
pub const IORT_ITS_GROUP_REVISION: u8 = 1;

pub const IORT_NODE_COHERENT: u32 = 0x00000001;
pub const IORT_MEMORY_ACCESS_COHERENCY: u8 = 1 << 0;
pub const IORT_MEMORY_ACCESS_ATTRIBUTES: u8 = 1 << 1;
pub const IORT_ID_SINGLE_MAPPING: u32 = 1 << 0;

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct Iort {
    pub node_count: u32_ne,
    pub node_offset: u32_ne,
    pub reserved: u32_ne,
}

impl Iort {
    pub fn new(node_count: u32) -> Self {
        Self {
            node_count: node_count.into(),
            node_offset: IORT_NODE_OFFSET.into(),
            reserved: 0.into(),
        }
    }
}

impl Table for Iort {
    const SIGNATURE: [u8; 4] = *b"IORT";
}

const_assert_eq!(size_of::<Iort>(), 12);
const_assert_eq!(IORT_NODE_OFFSET as usize, 48);

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct IortNodeHeader {
    pub node_type: u8,
    pub length: u16_ne,
    pub revision: u8,
    pub identifier: u32_ne,
    pub mapping_count: u32_ne,
    pub mapping_offset: u32_ne,
}

impl IortNodeHeader {
    pub fn new<T>(node_type: u8, revision: u8, identifier: u32, mapping_count: u32) -> Self {
        Self {
            node_type,
            length: (size_of::<T>() as u16).into(),
            revision,
            identifier: identifier.into(),
            mapping_count: mapping_count.into(),
            mapping_offset: if mapping_count == 0 {
                0.into()
            } else {
                (size_of::<T>() as u32).into()
            },
        }
    }
}

const_assert_eq!(size_of::<IortNodeHeader>(), 16);

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct IortMemoryAccessProperties {
    pub cache_coherency: u32_ne,
    pub hints: u8,
    pub reserved: u16_ne,
    pub memory_flags: u8,
}

impl IortMemoryAccessProperties {
    pub fn coherent() -> Self {
        Self {
            cache_coherency: IORT_NODE_COHERENT.into(),
            hints: 0,
            reserved: 0.into(),
            memory_flags: IORT_MEMORY_ACCESS_COHERENCY | IORT_MEMORY_ACCESS_ATTRIBUTES,
        }
    }
}

const_assert_eq!(size_of::<IortMemoryAccessProperties>(), 8);

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct IortPciRootComplex {
    pub header: IortNodeHeader,
    pub memory_properties: IortMemoryAccessProperties,
    pub ats_attribute: u32_ne,
    pub pci_segment_number: u32_ne,
    pub memory_address_limit: u8,
    pub reserved: [u8; 3],
}

impl IortPciRootComplex {
    /// Create a PCI Root Complex node. The `length` field in the header
    /// includes space for `mapping_count` trailing `IortIdMapping` entries,
    /// which must be appended separately after serializing this struct.
    pub fn new(identifier: u32, pci_segment_number: u16, mapping_count: u32) -> Self {
        let mut header = IortNodeHeader::new::<Self>(
            IORT_NODE_TYPE_PCI_ROOT_COMPLEX,
            IORT_PCI_ROOT_COMPLEX_REVISION,
            identifier,
            mapping_count,
        );
        // The node length must include the variable-length ID mapping array.
        let total =
            size_of::<Self>() as u16 + (mapping_count as u16) * size_of::<IortIdMapping>() as u16;
        header.length = total.into();
        Self {
            header,
            memory_properties: IortMemoryAccessProperties::coherent(),
            ats_attribute: 0.into(),
            pci_segment_number: u32::from(pci_segment_number).into(),
            memory_address_limit: 64,
            reserved: [0; 3],
        }
    }
}

const_assert_eq!(size_of::<IortPciRootComplex>(), 36);

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct IortIdMapping {
    pub input_base: u32_ne,
    pub id_count: u32_ne,
    pub output_base: u32_ne,
    pub output_reference: u32_ne,
    pub flags: u32_ne,
}

impl IortIdMapping {
    pub fn new(
        input_base: u32,
        id_count: u32,
        output_base: u32,
        output_reference: u32,
        flags: u32,
    ) -> Self {
        Self {
            input_base: input_base.into(),
            id_count: id_count.into(),
            output_base: output_base.into(),
            output_reference: output_reference.into(),
            flags: flags.into(),
        }
    }
}

const_assert_eq!(size_of::<IortIdMapping>(), 20);

/// ITS Group node. Followed by `its_count` u32 ITS identifiers.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct IortItsGroup {
    pub header: IortNodeHeader,
    pub its_count: u32_ne,
}

impl IortItsGroup {
    /// Create an ITS Group node. The `length` field in the header includes
    /// space for `its_count` trailing u32 ITS identifiers, which must be
    /// appended separately after serializing this struct.
    pub fn new(identifier: u32, its_count: u32) -> Self {
        let mut header = IortNodeHeader::new::<Self>(
            IORT_NODE_TYPE_ITS_GROUP,
            IORT_ITS_GROUP_REVISION,
            identifier,
            0,
        );
        // The node length must include the variable-length ITS ID array.
        let total = size_of::<Self>() as u16 + (its_count as u16) * 4;
        header.length = total.into();
        Self {
            header,
            its_count: its_count.into(),
        }
    }
}

const_assert_eq!(size_of::<IortItsGroup>(), 20);
