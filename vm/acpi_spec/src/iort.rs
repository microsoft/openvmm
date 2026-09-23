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
pub const IORT_NODE_TYPE_SMMUV3: u8 = 0x04;
pub const IORT_NODE_TYPE_RMR: u8 = 0x06;

pub const IORT_PCI_ROOT_COMPLEX_REVISION: u8 = 3;
pub const IORT_ITS_GROUP_REVISION: u8 = 1;
pub const IORT_SMMUV3_REVISION: u8 = 5;

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

/// SMMUv3 node flags.
pub const IORT_SMMUV3_FLAG_COHACC: u32 = 1 << 0;
/// `device_id_mapping_index` is valid (IORT rev E.e / node rev 5+).
pub const IORT_SMMUV3_FLAG_DEVICEID_VALID: u32 = 1 << 4;

/// SMMUv3 model: generic SMMU-v3.
pub const IORT_SMMUV3_MODEL_GENERIC: u32 = 0;

/// SMMUv3 node per IORT spec DEN0049E §E.4.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct IortSmmuV3 {
    pub header: IortNodeHeader,
    pub base_address: u64_ne,
    pub flags: u32_ne,
    pub reserved: u32_ne,
    pub vatos_address: u64_ne,
    pub model: u32_ne,
    pub event_gsiv: u32_ne,
    pub pri_gsiv: u32_ne,
    pub gerr_gsiv: u32_ne,
    pub sync_gsiv: u32_ne,
    pub proximity_domain: u32_ne,
    pub device_id_mapping_index: u32_ne,
}

impl IortSmmuV3 {
    /// Create an SMMUv3 node with COHACC set, wired SPI interrupts (GSIVs),
    /// and the specified number of ID mappings. The `length` field in the
    /// header includes space for `mapping_count` trailing `IortIdMapping`
    /// entries, which must be appended separately.
    pub fn new(
        identifier: u32,
        base_address: u64,
        mapping_count: u32,
        event_gsiv: u32,
        gerr_gsiv: u32,
    ) -> Self {
        Self::new_with_device_id_mapping(
            identifier,
            base_address,
            mapping_count,
            event_gsiv,
            gerr_gsiv,
            0,
        )
    }

    /// Create an SMMUv3 node with an explicit `device_id_mapping_index`.
    ///
    /// `device_id_mapping_index` selects which ID mapping entry Linux uses
    /// for the SMMU's own MSI domain lookup. That mapping must have the
    /// `IORT_ID_SINGLE_MAPPING` flag set. When set, the `DEVICEID_VALID`
    /// flag is automatically added to the node flags.
    pub fn new_with_device_id_mapping(
        identifier: u32,
        base_address: u64,
        mapping_count: u32,
        event_gsiv: u32,
        gerr_gsiv: u32,
        device_id_mapping_index: u32,
    ) -> Self {
        let mut header = IortNodeHeader::new::<Self>(
            IORT_NODE_TYPE_SMMUV3,
            IORT_SMMUV3_REVISION,
            identifier,
            mapping_count,
        );
        let total =
            size_of::<Self>() as u16 + (mapping_count as u16) * size_of::<IortIdMapping>() as u16;
        header.length = total.into();
        Self {
            header,
            base_address: base_address.into(),
            flags: (IORT_SMMUV3_FLAG_COHACC
                | if mapping_count > 0 {
                    IORT_SMMUV3_FLAG_DEVICEID_VALID
                } else {
                    0
                })
            .into(),
            reserved: 0.into(),
            vatos_address: 0.into(),
            model: IORT_SMMUV3_MODEL_GENERIC.into(),
            event_gsiv: event_gsiv.into(),
            pri_gsiv: 0.into(),
            gerr_gsiv: gerr_gsiv.into(),
            sync_gsiv: 0.into(),
            proximity_domain: 0.into(),
            device_id_mapping_index: device_id_mapping_index.into(),
        }
    }
}

const_assert_eq!(size_of::<IortSmmuV3>(), 68);

// --- RMR (Reserved Memory Range) node (IORT spec DEN0049E §E.5) ---

pub const IORT_RMR_REVISION: u8 = 3;

/// RMR flags: access privilege hint. When set, the OS may map the region
/// with elevated privileges (e.g., supervisor mode).
pub const IORT_RMR_ACCESS_PRIVILEGE: u32 = 1 << 0;
/// RMR flags: remap permitted. When clear, the region MUST be identity-
/// mapped (IOVA == physical address).
pub const IORT_RMR_REMAP_PERMITTED: u32 = 1 << 1;

/// RMR node header. Followed by `rmr_count` [`IortRmrDescriptor`] entries,
/// then `mapping_count` [`IortIdMapping`] entries.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct IortRmr {
    pub header: IortNodeHeader,
    pub flags: u32_ne,
    pub rmr_count: u32_ne,
    pub rmr_offset: u32_ne,
}

impl IortRmr {
    /// Create an RMR node with the given number of RMR descriptors and
    /// ID mappings. The `length` field includes space for the trailing
    /// descriptors and mappings, which must be appended separately.
    pub fn new(identifier: u32, flags: u32, rmr_count: u32, mapping_count: u32) -> Self {
        let mut header = IortNodeHeader::new::<Self>(
            IORT_NODE_TYPE_RMR,
            IORT_RMR_REVISION,
            identifier,
            mapping_count,
        );
        // Total node size: fixed header + ID mappings + RMR descriptors.
        // Accumulate in `usize` and convert to the on-wire `u16` `length`
        // field via `try_into`, so an oversized node fails loudly instead of
        // silently wrapping to a too-small length.
        let total = size_of::<Self>()
            + mapping_count as usize * size_of::<IortIdMapping>()
            + rmr_count as usize * size_of::<IortRmrDescriptor>();
        header.length = u16::try_from(total)
            .expect("IORT RMR node length exceeds u16")
            .into();
        // mapping_offset is immediately after the fixed header.
        if mapping_count > 0 {
            header.mapping_offset = (size_of::<Self>() as u32).into();
        }
        // rmr_offset comes after the ID mappings. Bounded by `total` (already
        // checked to fit `u16`), so the `u32` accumulation cannot overflow.
        let rmr_offset = if rmr_count > 0 {
            u32::try_from(size_of::<Self>() + mapping_count as usize * size_of::<IortIdMapping>())
                .expect("IORT RMR descriptor offset exceeds u32")
        } else {
            0
        };
        Self {
            header,
            flags: flags.into(),
            rmr_count: rmr_count.into(),
            rmr_offset: rmr_offset.into(),
        }
    }
}

const_assert_eq!(size_of::<IortRmr>(), 28);

/// RMR memory range descriptor.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct IortRmrDescriptor {
    pub base_address: u64_ne,
    pub length: u64_ne,
    pub reserved: u32_ne,
}

impl IortRmrDescriptor {
    pub fn new(base_address: u64, length: u64) -> Self {
        Self {
            base_address: base_address.into(),
            length: length.into(),
            reserved: 0.into(),
        }
    }
}

const_assert_eq!(size_of::<IortRmrDescriptor>(), 20);
