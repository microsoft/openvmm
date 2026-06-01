// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Interrupt Remapping Table Entry (IRTE) for the AMD IOMMU.
//!
//! Basic 32-bit format (GASup=0). Based on AMD IOMMU Specification Rev 3.11,
//! §2.2.5.1, Figure 15, Table 20.

use bitfield_struct::bitfield;
use inspect::Inspect;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// A 32-bit basic Interrupt Remapping Table Entry.
///
/// Used when GASup=0 (guest APIC not supported). Each IRTE specifies the
/// remapped interrupt vector, destination, and delivery mode.
#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
#[rustfmt::skip]
pub struct Irte {
    /// Remap enable — 1 = remap this interrupt, 0 = target abort.
    pub remap_en: bool,
    /// Suppress IO_PAGE_FAULT event logging for this interrupt.
    pub sup_iopf: bool,
    /// Interrupt type (delivery mode).
    #[bits(3)]
    pub int_type: u8,
    /// Request EOI.
    pub rq_eoi: bool,
    /// Destination mode — 0 = physical, 1 = logical.
    pub dm: bool,
    /// Guest mode — must be 0 for basic format.
    pub guest_mode: bool,
    /// APIC destination ID.
    #[bits(8)]
    pub destination: u8,
    /// Interrupt vector.
    #[bits(8)]
    pub vector: u8,
    #[bits(8)]
    _reserved: u32,
}

/// IRTE size in bytes (basic 32-bit format).
pub const IRTE_SIZE: usize = 4;

/// IRTE size in bytes (128-bit GA format, used when GASup=1 and GA_EN=1).
pub const IRTE_GA_SIZE: usize = 16;

/// A 128-bit Guest APIC Interrupt Remapping Table Entry (GA-format).
///
/// Used when GASup=1 and CONTROL\[GAEn\]=1. The low 64 bits contain remapping
/// fields with a wider (24-bit) destination. The high 64 bits hold the vector
/// and extended destination bits.
///
/// §2.2.5.2, Table 21.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct IrteGa {
    /// Low 64 bits.
    pub lo: IrteGaLo,
    /// High 64 bits.
    pub hi: IrteGaHi,
}

/// GA-format IRTE low 64 bits.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
#[rustfmt::skip]
pub struct IrteGaLo {
    /// Remap enable — 1 = remap this interrupt.
    pub remap_en: bool,
    /// Suppress IO_PAGE_FAULT event logging.
    pub sup_iopf: bool,
    /// Interrupt type (delivery mode).
    #[bits(3)]
    pub int_type: u8,
    /// Request EOI.
    pub rq_eoi: bool,
    /// Destination mode — 0 = physical, 1 = logical.
    pub dm: bool,
    /// Guest mode — 1 = guest VAPIC mode.
    pub guest_mode: bool,
    /// Destination APIC ID (low 24 bits).
    #[bits(24)]
    pub destination: u32,
    #[bits(32)]
    _reserved: u64,
}

/// GA-format IRTE high 64 bits.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
#[rustfmt::skip]
pub struct IrteGaHi {
    /// Interrupt vector.
    #[bits(8)]
    pub vector: u8,
    #[bits(48)]
    _reserved: u64,
    /// Destination APIC ID high bits [31:24].
    #[bits(8)]
    pub destination_hi: u8,
}

impl Irte {
    /// Get the interrupt type as a typed enum.
    pub fn interrupt_type(&self) -> IntType {
        IntType(self.int_type())
    }
}

open_enum! {
    /// Interrupt delivery type for the IRTE.
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum IntType: u8 {
        /// Fixed delivery mode.
        FIXED       = 0b000,
        /// Arbitrated (lowest priority) delivery mode.
        ARBITRATED  = 0b001,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_irte_size() {
        assert_eq!(size_of::<Irte>(), IRTE_SIZE);
        assert_eq!(IRTE_SIZE, 4);
    }

    #[test]
    fn test_irte_basic_roundtrip() {
        let irte = Irte::new()
            .with_remap_en(true)
            .with_int_type(IntType::FIXED.0)
            .with_dm(false) // physical destination
            .with_destination(3)
            .with_vector(0x40);
        assert!(irte.remap_en());
        assert_eq!(irte.interrupt_type(), IntType::FIXED);
        assert!(!irte.dm());
        assert_eq!(irte.destination(), 3);
        assert_eq!(irte.vector(), 0x40);
        assert!(!irte.guest_mode());
    }

    #[test]
    fn test_irte_disabled() {
        let irte = Irte::new().with_remap_en(false);
        assert!(!irte.remap_en());
    }

    #[test]
    fn test_irte_logical_destination() {
        let irte = Irte::new()
            .with_remap_en(true)
            .with_dm(true) // logical destination
            .with_destination(0xFF)
            .with_vector(0x30);
        assert!(irte.dm());
        assert_eq!(irte.destination(), 0xFF);
        assert_eq!(irte.vector(), 0x30);
    }

    #[test]
    fn test_irte_suppress_iopf() {
        let irte = Irte::new().with_remap_en(true).with_sup_iopf(true);
        assert!(irte.sup_iopf());
    }

    #[test]
    fn test_int_types() {
        assert_eq!(IntType::FIXED.0, 0);
        assert_eq!(IntType::ARBITRATED.0, 1);
    }

    #[test]
    fn test_irte_from_raw_u32() {
        // Construct a raw u32 representing an IRTE:
        // RemapEn=1, IntType=0 (Fixed), DM=0 (physical),
        // Destination=5, Vector=0x80
        let raw: u32 = 1       // RemapEn
            | (5 << 8)         // Destination = 5
            | (0x80 << 16); // Vector = 0x80
        let irte = Irte::from(raw);
        assert!(irte.remap_en());
        assert_eq!(irte.interrupt_type(), IntType::FIXED);
        assert!(!irte.dm());
        assert_eq!(irte.destination(), 5);
        assert_eq!(irte.vector(), 0x80);
    }
}
