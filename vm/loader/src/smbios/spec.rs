// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SMBIOS specification structure and constant definitions.
//!
//! These types mirror the on-the-wire SMBIOS structures from the DMTF SMBIOS
//! specification (and EDK2's `SmBios.h`). They contain no logic — only the
//! field layouts and spec-defined constant values. The table-building logic
//! lives in the parent module.
//!
//! The structures use plain `#[repr(C)]` with `zerocopy` little-endian integer
//! wrappers (which are alignment-1), so they are naturally packed and free of
//! padding while requiring no `unsafe`.

use static_assertions::const_assert_eq;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::LE;
use zerocopy::U16;
use zerocopy::U32;
use zerocopy::U64;

/// SMBIOS 3.0 64-bit entry point (`_SM3_`), 24 bytes.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout)]
pub struct Smbios30EntryPoint {
    /// Anchor string, `b"_SM3_"`.
    pub anchor: [u8; 5],
    /// Checksum: the 8-bit sum of all bytes in this structure is zero.
    pub checksum: u8,
    /// Length of this entry point, `0x18`.
    pub length: u8,
    /// SMBIOS major version.
    pub major: u8,
    /// SMBIOS minor version.
    pub minor: u8,
    /// SMBIOS docrev.
    pub docrev: u8,
    /// Entry point revision.
    pub revision: u8,
    /// Reserved, zero.
    pub reserved: u8,
    /// Maximum size of the structure table, in bytes.
    pub max_size: U32<LE>,
    /// Guest physical address of the first structure (Type 0).
    pub table_addr: U64<LE>,
}
const_assert_eq!(size_of::<Smbios30EntryPoint>(), 0x18);

/// SMBIOS Type 0 — BIOS Information, formatted area length `0x1a`.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout)]
pub struct SmbiosType0 {
    pub typ: u8,
    pub length: u8,
    pub handle: U16<LE>,
    pub vendor: u8,
    pub bios_version: u8,
    pub bios_segment: U16<LE>,
    pub bios_release_date: u8,
    pub bios_size: u8,
    pub characteristics: U64<LE>,
    pub characteristics_ext: [u8; 2],
    pub bios_major: u8,
    pub bios_minor: u8,
    pub ec_major: u8,
    pub ec_minor: u8,
    pub ext_rom_size: U16<LE>,
}
const_assert_eq!(size_of::<SmbiosType0>(), 0x1a);

/// SMBIOS Type 1 — System Information, formatted area length `0x1b`.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout)]
pub struct SmbiosType1 {
    pub typ: u8,
    pub length: u8,
    pub handle: U16<LE>,
    pub manufacturer: u8,
    pub product_name: u8,
    pub version: u8,
    pub serial_number: u8,
    pub uuid: [u8; 16],
    pub wake_up_type: u8,
    pub sku_number: u8,
    pub family: u8,
}
const_assert_eq!(size_of::<SmbiosType1>(), 0x1b);

/// SMBIOS Type 127 — End of Table, 4 bytes.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout)]
pub struct SmbiosType127 {
    pub typ: u8,
    pub length: u8,
    pub handle: U16<LE>,
}
const_assert_eq!(size_of::<SmbiosType127>(), 0x4);

/// Type 0 BIOS characteristics: PCI supported (bit 7).
pub const BIOS_CHARACTERISTICS_PCI_SUPPORTED: u64 = 1 << 7;
/// Type 0 BIOS characteristics extension byte 1: ACPI supported (bit 0).
pub const BIOS_CHARACTERISTICS_EXT1_ACPI: u8 = 1 << 0;
/// Type 0 BIOS characteristics extension byte 2: virtual machine (bit 4).
pub const BIOS_CHARACTERISTICS_EXT2_VM: u8 = 1 << 4;
/// Type 1 wake-up type: power switch.
pub const WAKE_UP_TYPE_POWER_SWITCH: u8 = 0x06;
