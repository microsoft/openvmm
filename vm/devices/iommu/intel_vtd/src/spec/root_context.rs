// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Root Table and Context Table entry types for the Intel VT-d IOMMU.
//!
//! Based on Intel VT-d Specification Rev 4.1, §9.1 (Root Entry) and §9.3
//! (Context Entry). Legacy mode only.
//!
//! The VT-d uses a two-level device lookup:
//! - Root Table: 4KB, 256 entries indexed by PCI bus number.
//! - Context Table: 4KB per bus, 256 entries indexed by devfn (device:function).

use bitfield_struct::bitfield;
use inspect::Inspect;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Root Table Entry (128-bit, §9.1).
///
/// Each entry maps a PCI bus number to a context table. The root table has 256
/// entries (one per bus), totaling 4KB.
///
/// Layout (legacy mode):
/// - Bits 0: P (present)
/// - Bits 11:1: reserved
/// - Bits 63:12: CTP (context table pointer, 4KB-aligned)
/// - Bits 127:64: reserved (upper 64 bits unused in legacy mode)
#[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct RootEntry {
    /// Low 64 bits: present bit and context table pointer.
    pub lo: RootEntryLo,
    /// High 64 bits: reserved in legacy mode.
    pub hi: u64,
}

/// Number of entries in the root table (one per PCI bus).
pub const ROOT_TABLE_ENTRIES: usize = 256;

/// Root Entry — low 64 bits.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
#[rustfmt::skip]
pub struct RootEntryLo {
    /// Present — 1 = this entry is valid.
    pub p: bool,
    #[bits(11)]
    _reserved: u64,
    /// Context Table Pointer, bits [63:12]. 4KB-aligned.
    #[bits(52)]
    pub ctp: u64,
}

impl RootEntryLo {
    /// Get the full context table physical address (bits 63:12 shifted).
    pub fn context_table_address(&self) -> u64 {
        self.ctp() << 12
    }
}

/// Context Table Entry (128-bit, §9.3).
///
/// Each entry maps a PCI device/function to a second-level page table and
/// domain. The context table has 256 entries (32 devices × 8 functions = 256
/// devfn combinations per bus), totaling 4KB.
///
/// Layout:
/// - Low 64 bits:
///   - Bit 0: P (present)
///   - Bit 1: FPD (fault processing disable)
///   - Bits 3:2: TT (translation type)
///   - Bits 11:4: reserved
///   - Bits 63:12: SSPTPTR (second-stage page table pointer, 4KB-aligned)
/// - High 64 bits:
///   - Bits 66:64 (2:0): AW (address width)
///   - Bits 71:67 (7:3): reserved
///   - Bits 87:72 (23:8): DID (domain ID)
///   - Bits 127:88 (63:24): reserved
#[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct ContextEntry {
    /// Low 64 bits: present, FPD, translation type, page table pointer.
    pub lo: ContextEntryLo,
    /// High 64 bits: address width, domain ID.
    pub hi: ContextEntryHi,
}

/// Number of entries per context table.
pub const CONTEXT_TABLE_ENTRIES: usize = 256;

/// Context Entry — low 64 bits.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
#[rustfmt::skip]
pub struct ContextEntryLo {
    /// Present — 1 = this entry is valid.
    pub p: bool,
    /// Fault Processing Disable — 1 = suppress fault recording for this device.
    pub fpd: bool,
    /// Translation Type.
    /// 00 = untranslated requests only (second-level translation).
    /// 01 = all requests (both translated and untranslated, second-level).
    /// 10 = pass-through (IOVA = GPA, identity mapping).
    /// 11 = reserved.
    #[bits(2)]
    pub tt: u8,
    #[bits(8)]
    _reserved: u64,
    /// Second-Stage Page Translation Pointer, bits [63:12]. 4KB-aligned.
    /// Points to the root of the second-level page table hierarchy.
    #[bits(52)]
    pub ssptptr: u64,
}

impl ContextEntryLo {
    /// Get the full second-stage page table pointer address (bits 63:12 shifted).
    pub fn page_table_address(&self) -> u64 {
        self.ssptptr() << 12
    }
}

/// Context Entry — high 64 bits.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
#[rustfmt::skip]
pub struct ContextEntryHi {
    /// Address Width — number of levels in the second-level page table.
    /// 001 = 39-bit / 3-level, 010 = 48-bit / 4-level, 011 = 57-bit / 5-level.
    #[bits(3)]
    pub aw: u8,
    #[bits(5)]
    _reserved1: u64,
    /// Domain ID (16-bit). Identifies the domain for TLB invalidation.
    #[bits(16)]
    pub did: u16,
    #[bits(40)]
    _reserved2: u64,
}

open_enum! {
    /// Translation type for context entries (§9.3).
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum TranslationType: u8 {
        /// Untranslated requests only — second-level translation applied.
        UNTRANSLATED_ONLY       = 0b00,
        /// All requests — second-level translation applied to both translated
        /// and untranslated requests.
        ALL                     = 0b01,
        /// Pass-through — IOVA = GPA (identity mapping).
        PASS_THROUGH            = 0b10,
    }
}

open_enum! {
    /// Address width for second-level page tables (§9.3).
    ///
    /// Determines the number of page table levels walked.
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum AddressWidth: u8 {
        /// 39-bit (3-level page table). AGAW = 39 bits.
        AW_39BIT    = 0b001,
        /// 48-bit (4-level page table). AGAW = 48 bits.
        AW_48BIT    = 0b010,
        /// 57-bit (5-level page table). AGAW = 57 bits.
        AW_57BIT    = 0b011,
    }
}

impl AddressWidth {
    /// Get the number of page table levels for this address width.
    pub fn levels(&self) -> Option<u8> {
        match *self {
            AddressWidth::AW_39BIT => Some(3),
            AddressWidth::AW_48BIT => Some(4),
            AddressWidth::AW_57BIT => Some(5),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_address_width_levels() {
        assert_eq!(AddressWidth::AW_39BIT.levels(), Some(3));
        assert_eq!(AddressWidth::AW_48BIT.levels(), Some(4));
        assert_eq!(AddressWidth::AW_57BIT.levels(), Some(5));
        assert_eq!(AddressWidth(0).levels(), None);
        assert_eq!(AddressWidth(0b100).levels(), None);
    }
}
