// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! DMAR (DMA Remapping Reporting Structure) types for Intel VT-d discovery.
//!
//! The DMAR ACPI table describes Intel VT-d remapping hardware to the guest
//! OS. It contains one or more DRHD (DMA Remapping Hardware Unit Definition)
//! structures, each describing a single VT-d remapping unit: its MMIO base,
//! PCI segment, and the set of devices behind it via device scope entries.
//!
//! Reference: Intel Virtualization Technology for Directed I/O Architecture
//! Specification, Document 774206, §8.

use super::Table;
use crate::packed_nums::*;
use core::mem::size_of;
use static_assertions::const_assert_eq;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::Unaligned;

/// DMAR table revision (VT-d spec §8.1).
pub const DMAR_REVISION: u8 = 1;

/// DMAR flags: interrupt remapping supported (§8.1, Table 8-1).
pub const DMAR_FLAGS_INTR_REMAP: u8 = 0x01;

/// DRHD structure type (§8.2).
pub const DMAR_TYPE_DRHD: u16 = 0x0000;

/// DRHD flags: INCLUDE_PCI_ALL (§8.2).
pub const DRHD_FLAGS_INCLUDE_PCI_ALL: u8 = 0x01;

/// Device scope entry type: PCI endpoint device (§8.3.1, Table 8-5).
pub const DEVICE_SCOPE_PCI_ENDPOINT: u8 = 0x01;

/// Device scope entry type: PCI sub-hierarchy (§8.3.1, Table 8-5).
pub const DEVICE_SCOPE_PCI_SUB_HIERARCHY: u8 = 0x02;

/// Device scope entry type: IOAPIC (§8.3.1, Table 8-5).
pub const DEVICE_SCOPE_IOAPIC: u8 = 0x03;

/// Device scope entry type: HPET (§8.3.1, Table 8-5).
pub const DEVICE_SCOPE_HPET: u8 = 0x04;

/// DMAR fixed table header (follows the standard ACPI `Header`).
///
/// The DMAR table starts with the standard 36-byte ACPI header, followed by
/// this 12-byte structure (HAW + flags + reserved), followed by one or more
/// remapping structures (DRHD, RMRR, etc.).
///
/// Reference: VT-d spec §8.1, Table 8-1.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct Dmar {
    /// Host Address Width: maximum DMA physical addressability.
    /// Value is N where address width = N + 1. E.g., 0x2F = 48-bit.
    pub host_address_width: u8,
    /// Flags (see `DMAR_FLAGS_*` constants).
    pub flags: u8,
    /// Reserved, must be zero.
    pub reserved: [u8; 10],
}

impl Dmar {
    /// Create a new DMAR header with the given host address width and flags.
    pub fn new(host_address_width: u8, flags: u8) -> Self {
        Self {
            host_address_width,
            flags,
            reserved: [0; 10],
        }
    }
}

impl Table for Dmar {
    const SIGNATURE: [u8; 4] = *b"DMAR";
}

const_assert_eq!(size_of::<Dmar>(), 12);

/// DRHD (DMA Remapping Hardware Unit Definition) structure (§8.2).
///
/// Describes a single VT-d remapping unit. The structure is followed by
/// zero or more device scope entries.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct DmarDrhd {
    /// Structure type: always [`DMAR_TYPE_DRHD`].
    pub structure_type: u16_ne,
    /// Length of the entire DRHD structure including device scope entries.
    pub length: u16_ne,
    /// Flags (see `DRHD_FLAGS_*` constants).
    pub flags: u8,
    /// Size: register set size as 2^N 4KB pages. 0 = 1 page (4KB).
    pub size: u8,
    /// PCI segment number.
    pub segment_number: u16_ne,
    /// Register base address of the remapping unit (MMIO base).
    pub register_base_address: u64_ne,
}

impl DmarDrhd {
    /// Create a new DRHD structure.
    pub fn new(flags: u8, segment_number: u16, register_base_address: u64) -> Self {
        Self {
            structure_type: DMAR_TYPE_DRHD.into(),
            length: (size_of::<Self>() as u16).into(),
            flags,
            size: 0, // 1 page (4KB)
            segment_number: segment_number.into(),
            register_base_address: register_base_address.into(),
        }
    }

    /// Set the total length (header + device scope entries).
    pub fn with_length(mut self, length: u16) -> Self {
        self.length = length.into();
        self
    }
}

const_assert_eq!(size_of::<DmarDrhd>(), 16);

/// Device Scope entry (§8.3.1).
///
/// A device scope entry identifies a PCI device or sub-hierarchy covered
/// by the parent DRHD. The entry is followed by one or more `DmarDevicePath`
/// entries describing the PCI path from the host bridge to the device.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct DmarDeviceScope {
    /// Device scope type (see `DEVICE_SCOPE_*` constants).
    pub device_scope_type: u8,
    /// Length of this device scope entry including path entries.
    pub length: u8,
    /// Reserved, must be zero.
    pub reserved: u16_ne,
    /// Enumeration ID. For IOAPIC/HPET scopes, this is the I/O APIC ID
    /// or HPET number. For PCI scopes, must be 0.
    pub enumeration_id: u8,
    /// Start bus number for PCI sub-hierarchy scopes.
    pub start_bus_number: u8,
}

impl DmarDeviceScope {
    /// Create a device scope entry with the given type and start bus number.
    ///
    /// The entry is sized for a single path entry (one `DmarDevicePath`).
    pub fn new(device_scope_type: u8, start_bus_number: u8) -> Self {
        Self {
            device_scope_type,
            // Length = 6 (header) + 2 (one path entry)
            length: (size_of::<Self>() + size_of::<DmarDevicePath>()) as u8,
            reserved: 0.into(),
            enumeration_id: 0,
            start_bus_number,
        }
    }
}

const_assert_eq!(size_of::<DmarDeviceScope>(), 6);

/// Device path entry within a device scope (§8.3.1).
///
/// Each path entry is a (device, function) pair describing one hop from
/// the start bus to the target device.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
pub struct DmarDevicePath {
    /// PCI device number.
    pub device: u8,
    /// PCI function number.
    pub function: u8,
}

const_assert_eq!(size_of::<DmarDevicePath>(), 2);
