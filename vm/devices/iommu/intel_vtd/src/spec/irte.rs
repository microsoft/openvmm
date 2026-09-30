// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Interrupt Remapping Table Entry (IRTE) for the Intel VT-d IOMMU.
//!
//! Based on Intel VT-d Specification Rev 4.1, §9.9. VT-d IRTEs are 128-bit
//! (16 bytes), indexed by the interrupt index extracted from remappable-format
//! MSI address/data. The IRTE specifies the remapped interrupt vector,
//! destination, delivery mode, and source validation fields.

use bitfield_struct::bitfield;
use inspect::Inspect;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// A 128-bit Interrupt Remapping Table Entry (§9.9).
///
/// Layout:
/// - Low 64 bits: remapping control fields, vector, destination
/// - High 64 bits: source validation (SID, SQ, SVT)
#[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct Irte {
    /// Low 64 bits: present, FPD, mode, vector, destination.
    pub lo: IrteLo,
    /// High 64 bits: source ID, source qualifier, source validation type.
    pub hi: IrteHi,
}

/// IRTE — low 64 bits.
///
/// Contains the interrupt remapping control fields:
/// - P (present), FPD, DM, RH, TM, DLM, IM, Vector, DST
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
#[rustfmt::skip]
pub struct IrteLo {
    /// Present — 1 = this IRTE is valid.
    pub p: bool,
    /// Fault Processing Disable — 1 = suppress fault recording for this entry.
    pub fpd: bool,
    /// Destination Mode — 0 = physical, 1 = logical.
    pub dm: bool,
    /// Redirection Hint — 0 = directed to single destination,
    /// 1 = may be redirected (for lowest priority delivery).
    pub rh: bool,
    /// Trigger Mode — 0 = edge, 1 = level.
    pub tm: bool,
    /// Delivery Mode (3 bits).
    #[bits(3)]
    pub dlm: u8,
    /// Available for software use (4 bits).
    #[bits(4)]
    pub avail: u8,
    #[bits(3)]
    _reserved1: u64,
    /// IRTE Mode — 0 = remapped interrupt, 1 = posted interrupt (not supported).
    pub im: bool,
    /// Interrupt Vector (8 bits).
    #[bits(8)]
    pub vector: u8,
    #[bits(8)]
    _reserved2: u64,
    /// Destination ID (32 bits).
    /// - xAPIC mode (EIME=0): only bits 47:40 (8-bit APIC ID) are significant.
    /// - x2APIC mode (EIME=1): full 32-bit APIC ID.
    #[bits(32)]
    pub dst: u32,
}

impl IrteLo {
    /// Get the xAPIC destination (8-bit, from bits 47:40 of the DST field).
    ///
    /// In xAPIC mode (EIME=0), only bits 15:8 of the 32-bit DST field are
    /// used as the APIC destination ID.
    pub fn xapic_destination(&self) -> u8 {
        ((self.dst() >> 8) & 0xFF) as u8
    }
}

/// IRTE — high 64 bits.
///
/// Contains source validation fields for verifying that an interrupt comes
/// from the expected device.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
#[rustfmt::skip]
pub struct IrteHi {
    /// Source Identifier (16 bits) — expected source BDF or bus range.
    #[bits(16)]
    pub sid: u16,
    /// Source-ID Qualifier (2 bits) — controls how SID matching works.
    #[bits(2)]
    pub sq: u8,
    /// Source Validation Type (2 bits).
    /// 00 = no validation, 01 = verify SID, 10 = verify bus range.
    #[bits(2)]
    pub svt: u8,
    #[bits(44)]
    _reserved: u64,
}

open_enum! {
    /// Delivery mode for interrupt remapping (§9.9).
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum DeliveryMode: u8 {
        /// Fixed delivery to the specified destination(s).
        FIXED           = 0b000,
        /// Lowest priority delivery.
        LOWEST_PRIORITY = 0b001,
        /// SMI (System Management Interrupt).
        SMI             = 0b010,
        /// NMI (Non-Maskable Interrupt).
        NMI             = 0b100,
        /// INIT.
        INIT            = 0b101,
        /// ExtINT (external interrupt).
        EXTINTR         = 0b111,
    }
}

open_enum! {
    /// Source validation type for IRTE (§9.9).
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum SourceValidationType: u8 {
        /// No source validation — any device can use this IRTE.
        NONE            = 0b00,
        /// Verify source ID matches IRTE.SID (per SQ mask).
        VERIFY_SID      = 0b01,
        /// Verify source bus number is in range [SID[7:0], SID[15:8]].
        VERIFY_BUS_RANGE = 0b10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xapic_destination() {
        let lo = IrteLo::new().with_dst(0x0000_0300);
        assert_eq!(lo.xapic_destination(), 3);
    }
}
