// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Intel MultiProcessor Specification 1.4 structure and constant definitions.
//!
//! These types mirror the MP floating pointer, the MP configuration table
//! header, and the base configuration table entries from chapter 4 of the
//! Intel MultiProcessor Specification, version 1.4. They contain no logic,
//! only the field layouts and spec-defined values. The table-building logic
//! lives in the parent module.
//!
//! As in the SMBIOS definitions, the structures use plain `#[repr(C)]` with
//! `zerocopy` little-endian integer wrappers, which are alignment-1, so they
//! are naturally packed and free of padding.

use static_assertions::const_assert_eq;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::LE;
use zerocopy::U16;
use zerocopy::U32;

/// `SPEC_REV` of the floating pointer and the configuration table header for
/// version 1.4 (Tables 4-1 and 4-2).
pub const SPEC_REV_1_4: u8 = 4;

/// The size of one floating pointer `LENGTH` unit, a 16-byte paragraph.
pub const PARAGRAPH_SIZE: usize = 16;

/// MP floating pointer structure (section 4.1, Table 4-1).
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FloatingPointer {
    /// [`FLOATING_POINTER_SIGNATURE`].
    pub signature: [u8; 4],
    /// Physical address of the MP configuration table.
    pub config_table_address: U32<LE>,
    /// Length of this structure in 16-byte paragraphs.
    pub length: u8,
    /// MP specification revision.
    pub spec_rev: u8,
    /// All bytes of the structure add up to zero.
    pub checksum: u8,
    /// MP feature information byte 1: zero when a configuration table is
    /// present, otherwise a default configuration type.
    pub config_type: u8,
    /// MP feature information byte 2: bit 7 (IMCRP) selects PIC mode;
    /// clear selects virtual wire mode.
    pub features: u8,
    /// MP feature information bytes 3-5, reserved and zero.
    pub reserved: [u8; 3],
}
const_assert_eq!(size_of::<FloatingPointer>(), 16);

/// Floating pointer signature (Table 4-1).
pub const FLOATING_POINTER_SIGNATURE: [u8; 4] = *b"_MP_";

/// MP configuration table header (section 4.2, Table 4-2).
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ConfigTableHeader {
    /// [`CONFIG_TABLE_SIGNATURE`].
    pub signature: [u8; 4],
    /// Length of the base table in bytes, including this header.
    pub base_table_length: U16<LE>,
    /// MP specification revision.
    pub spec_rev: u8,
    /// All bytes of the base table add up to zero.
    pub checksum: u8,
    /// Space-padded system manufacturer string.
    pub oem_id: [u8; 8],
    /// Space-padded product family string.
    pub product_id: [u8; 12],
    /// Physical address of an optional OEM table, or zero.
    pub oem_table_pointer: U32<LE>,
    /// Size of the OEM table, or zero.
    pub oem_table_size: U16<LE>,
    /// Number of entries in the variable portion of the base table.
    pub entry_count: U16<LE>,
    /// Address through which each processor accesses its local APIC.
    pub local_apic_address: U32<LE>,
    /// Length of the extended table entries, or zero.
    pub extended_table_length: U16<LE>,
    /// Checksum of the extended table entries, or zero.
    pub extended_table_checksum: u8,
    /// Reserved.
    pub reserved: u8,
}
const_assert_eq!(size_of::<ConfigTableHeader>(), 44);

/// Configuration table signature (Table 4-2).
pub const CONFIG_TABLE_SIGNATURE: [u8; 4] = *b"PCMP";

/// Processor entry type (Table 4-3).
pub const ENTRY_TYPE_PROCESSOR: u8 = 0;
/// Bus entry type (Table 4-3).
pub const ENTRY_TYPE_BUS: u8 = 1;
/// I/O APIC entry type (Table 4-3).
pub const ENTRY_TYPE_IO_APIC: u8 = 2;
/// I/O interrupt assignment entry type (Table 4-3).
pub const ENTRY_TYPE_IO_INTERRUPT: u8 = 3;

/// Processor entry (section 4.3.1, Table 4-4).
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ProcessorEntry {
    /// [`ENTRY_TYPE_PROCESSOR`].
    pub entry_type: u8,
    /// Local APIC ID.
    pub local_apic_id: u8,
    /// Bits 0-7 of the local APIC version register.
    pub local_apic_version: u8,
    /// [`CPU_FLAG_ENABLED`] and [`CPU_FLAG_BOOTSTRAP`].
    pub cpu_flags: u8,
    /// CPUID stepping, model, and family, or zero.
    pub cpu_signature: U32<LE>,
    /// CPUID feature flags, or zero.
    pub feature_flags: U32<LE>,
    /// Reserved.
    pub reserved: [u8; 8],
}
const_assert_eq!(size_of::<ProcessorEntry>(), 20);

/// CPU flags bit EN: the processor is usable (Table 4-4).
pub const CPU_FLAG_ENABLED: u8 = 1 << 0;
/// CPU flags bit BP: the processor is the bootstrap processor (Table 4-4).
pub const CPU_FLAG_BOOTSTRAP: u8 = 1 << 1;

/// Bus entry (section 4.3.2, Table 4-7).
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct BusEntry {
    /// [`ENTRY_TYPE_BUS`].
    pub entry_type: u8,
    /// Bus ID that interrupt entries refer to.
    pub bus_id: u8,
    /// Space-padded bus type string.
    pub bus_type: [u8; 6],
}
const_assert_eq!(size_of::<BusEntry>(), 8);

/// Bus type string of an ISA bus (Table 4-8).
pub const BUS_TYPE_ISA: [u8; 6] = *b"ISA   ";

/// I/O APIC entry (section 4.3.3, Table 4-9).
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct IoApicEntry {
    /// [`ENTRY_TYPE_IO_APIC`].
    pub entry_type: u8,
    /// I/O APIC ID.
    pub io_apic_id: u8,
    /// Bits 0-7 of the I/O APIC version register.
    pub io_apic_version: u8,
    /// [`IO_APIC_FLAG_ENABLED`].
    pub flags: u8,
    /// Base address of the I/O APIC.
    pub address: U32<LE>,
}
const_assert_eq!(size_of::<IoApicEntry>(), 8);

/// I/O APIC flags bit EN: the I/O APIC is usable (Table 4-9).
pub const IO_APIC_FLAG_ENABLED: u8 = 1 << 0;

/// I/O interrupt assignment entry (section 4.3.4, Table 4-10).
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct IoInterruptEntry {
    /// [`ENTRY_TYPE_IO_INTERRUPT`].
    pub entry_type: u8,
    /// Interrupt type (Table 4-11).
    pub interrupt_type: u8,
    /// Polarity (PO, bits 0-1) and trigger mode (EL, bits 2-3). Zero
    /// conforms to the source bus, which for ISA is active-high and
    /// edge-triggered.
    pub flags: U16<LE>,
    /// Bus ID of the interrupt source.
    pub source_bus_id: u8,
    /// IRQ of the interrupt source on its bus.
    pub source_bus_irq: u8,
    /// Destination I/O APIC ID.
    pub destination_io_apic_id: u8,
    /// Destination I/O APIC input pin (INTIN#).
    pub destination_io_apic_intin: u8,
}
const_assert_eq!(size_of::<IoInterruptEntry>(), 8);

/// Interrupt type INT: a vectored interrupt whose vector comes from the
/// I/O APIC redirection table (Table 4-11).
pub const INTERRUPT_TYPE_INT: u8 = 0;
/// Polarity PO = 01b, active high (Table 4-10).
pub const INTERRUPT_FLAG_ACTIVE_HIGH: u16 = 0b01;
/// Trigger mode EL = 11b, level-triggered (Table 4-10).
pub const INTERRUPT_FLAG_LEVEL_TRIGGERED: u16 = 0b11 << 2;
