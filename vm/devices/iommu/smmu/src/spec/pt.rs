// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! AArch64 VMSAv8 stage 1 page table descriptor definitions.
//!
//! The SMMU uses the same page table format as AArch64 PE stage 1 translation.
//! These are the standard ARMv8 translation table descriptors defined in the
//! Arm Architecture Reference Manual (DDI 0487).

use bitfield_struct::bitfield;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// A 64-bit page table descriptor.
///
/// The interpretation depends on the level and the Type bit:
/// - Level 0-2, Type=1: Table descriptor (points to next-level table)
/// - Level 1-2, Type=0: Block descriptor (maps a large region)
/// - Level 3, Type=1: Page descriptor (maps a single page)
/// - Level 3, Type=0: Reserved (invalid)
/// - Valid=0: Invalid/fault entry
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PtDesc {
    /// Valid bit. 0 = fault entry.
    pub valid: bool,
    /// Descriptor type. 1 = table/page, 0 = block (or reserved at L3).
    pub desc_type: bool,
    /// Memory attribute index (indexes into MAIR).
    #[bits(3)]
    pub attr_index: u8,
    /// Non-secure bit.
    pub ns: bool,
    /// Access permissions.
    #[bits(2)]
    pub ap: u8,
    /// Shareability.
    #[bits(2)]
    pub sh: u8,
    /// Access flag. Must be 1 to avoid AF faults (when HTTU not supported).
    pub af: bool,
    /// Not-global (if 1, uses ASID for TLB matching).
    pub ng: bool,
    /// Output address / next-level table address bits `[47:12]`.
    /// For 4KB granule: block at L1 uses `[47:30]`, block at L2 uses `[47:21]`,
    /// page at L3 uses `[47:12]`.
    #[bits(36)]
    pub addr_bits: u64,
    /// Reserved / upper attributes bits `[49:48]`.
    #[bits(2)]
    _reserved_upper: u64,
    /// Guarded page.
    pub gp: bool,
    /// Dirty bit modifier.
    pub dbm: bool,
    /// Contiguous hint.
    pub contiguous: bool,
    /// Privileged execute-never.
    pub pxn: bool,
    /// Unprivileged execute-never (or XN for EL2/EL3).
    pub uxn: bool,
    /// Software use / PBHA.
    #[bits(4)]
    pub sw_use: u8,
    /// Ignored / PBHA.
    #[bits(5)]
    pub ignored_upper: u8,
}

impl PtDesc {
    /// Returns true if this is a valid entry.
    pub fn is_valid(&self) -> bool {
        self.valid()
    }

    /// Returns true if this is a table descriptor (levels 0-2) or page
    /// descriptor (level 3). Type bit = 1.
    pub fn is_table(&self) -> bool {
        self.valid() && self.desc_type()
    }

    /// Returns true if this is a block descriptor (levels 1-2).
    /// Valid=1 and Type=0.
    pub fn is_block(&self) -> bool {
        self.valid() && !self.desc_type()
    }

    /// Returns true if this is a page descriptor at level 3.
    /// At L3, Valid=1 and Type=1 means page. Type=0 is reserved/fault.
    pub fn is_page_at_l3(&self) -> bool {
        self.valid() && self.desc_type()
    }

    /// Returns the output address for a 4KB granule.
    ///
    /// For table descriptors: the next-level table address (bits `[47:12]`).
    /// For block descriptors at L1: bits `[47:30]` (1GB block).
    /// For block descriptors at L2: bits `[47:21]` (2MB block).
    /// For page descriptors at L3: bits `[47:12]` (4KB page).
    pub fn output_address_4k(&self, level: u8) -> u64 {
        let raw = self.addr_bits() << 12;
        match level {
            0 => raw, // table only at L0 for 4K
            1 => {
                if self.is_block() {
                    raw & !((1u64 << 30) - 1) // 1GB aligned
                } else {
                    raw // table address
                }
            }
            2 => {
                if self.is_block() {
                    raw & !((1u64 << 21) - 1) // 2MB aligned
                } else {
                    raw // table address
                }
            }
            3 => raw, // page address, 4KB aligned
            _ => raw,
        }
    }

    /// Returns the output address for a 16KB granule.
    pub fn output_address_16k(&self, level: u8) -> u64 {
        let raw = self.addr_bits() << 12;
        match level {
            // L1 block: 64GB (bits [47:36])
            1 => {
                if self.is_block() {
                    raw & !((1u64 << 36) - 1)
                } else {
                    raw
                }
            }
            // L2 block: 32MB (bits [47:25])
            2 => {
                if self.is_block() {
                    raw & !((1u64 << 25) - 1)
                } else {
                    raw
                }
            }
            // L3 page: 16KB aligned — clear RES0 bits [13:12]
            3 => raw & !((1u64 << 14) - 1),
            _ => raw,
        }
    }

    /// Returns the output address for a 64KB granule.
    pub fn output_address_64k(&self, level: u8) -> u64 {
        let raw = self.addr_bits() << 12;
        match level {
            // L2 block: 512MB (bits [47:29])
            2 => {
                if self.is_block() {
                    raw & !((1u64 << 29) - 1)
                } else {
                    raw
                }
            }
            // L3 page: 64KB aligned — clear RES0 bits [15:12]
            3 => raw & !((1u64 << 16) - 1),
            _ => raw,
        }
    }

    /// Returns the next-level table address (for table descriptors),
    /// masked to the given granule alignment. Bits below `page_shift`
    /// are RES0 in the descriptor and are cleared.
    pub fn next_table_addr(&self, page_shift: u8) -> u64 {
        (self.addr_bits() << 12) & !((1u64 << page_shift) - 1)
    }
}

open_enum! {
    /// Access permission bits (AP`[2:1]`).
    pub enum ApBits: u8 {
        /// EL1 R/W, EL0 no access.
        RW_EL1 = 0b00,
        /// EL1 R/W, EL0 R/W.
        RW_ANY = 0b01,
        /// EL1 R/O, EL0 no access.
        RO_EL1 = 0b10,
        /// EL1 R/O, EL0 R/O.
        RO_ANY = 0b11,
    }
}

impl ApBits {
    /// Returns true if the access permissions allow writes.
    pub fn allows_write(self) -> bool {
        match self {
            Self::RW_EL1 | Self::RW_ANY => true,
            Self::RO_EL1 | Self::RO_ANY => false,
            _ => false,
        }
    }

    /// Returns true if the access permissions allow reads (always true for
    /// valid permissions).
    pub fn allows_read(self) -> bool {
        true
    }
}

open_enum! {
    /// Shareability field values.
    pub enum Shareability: u8 {
        /// Non-shareable.
        NON_SHAREABLE = 0b00,
        /// Outer shareable.
        OUTER_SHAREABLE = 0b10,
        /// Inner shareable.
        INNER_SHAREABLE = 0b11,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pt_desc_invalid() {
        let desc = PtDesc::from(0u64);
        assert!(!desc.is_valid());
        assert!(!desc.is_table());
        assert!(!desc.is_block());
    }

    #[test]
    fn test_pt_desc_table() {
        // Valid=1, Type=1 → table descriptor
        let desc = PtDesc::new().with_valid(true).with_desc_type(true);
        assert!(desc.is_valid());
        assert!(desc.is_table());
        assert!(!desc.is_block());
    }

    #[test]
    fn test_pt_desc_block() {
        // Valid=1, Type=0 → block descriptor
        let desc = PtDesc::new().with_valid(true).with_desc_type(false);
        assert!(desc.is_valid());
        assert!(!desc.is_table());
        assert!(desc.is_block());
    }

    #[test]
    fn test_pt_desc_page_at_l3() {
        // At L3: Valid=1, Type=1 → page descriptor
        let desc = PtDesc::new().with_valid(true).with_desc_type(true);
        assert!(desc.is_page_at_l3());
    }

    #[test]
    fn test_pt_desc_4k_page_address() {
        // 4K page at L3: output address at bits [47:12]
        let page_addr: u64 = 0x4000_1000;
        let desc = PtDesc::new()
            .with_valid(true)
            .with_desc_type(true)
            .with_addr_bits(page_addr >> 12);

        assert_eq!(desc.output_address_4k(3), page_addr);
    }

    #[test]
    fn test_pt_desc_4k_l2_block_address() {
        // 2MB block at L2: output address at bits [47:21]
        let block_addr: u64 = 0x4020_0000; // 2MB aligned
        let desc = PtDesc::new()
            .with_valid(true)
            .with_desc_type(false)
            .with_addr_bits(block_addr >> 12);

        assert_eq!(desc.output_address_4k(2), block_addr);
    }

    #[test]
    fn test_pt_desc_4k_l1_block_address() {
        // 1GB block at L1: output address at bits [47:30]
        let block_addr: u64 = 0x4000_0000; // 1GB aligned
        let desc = PtDesc::new()
            .with_valid(true)
            .with_desc_type(false)
            .with_addr_bits(block_addr >> 12);

        assert_eq!(desc.output_address_4k(1), block_addr);
    }

    #[test]
    fn test_pt_desc_table_next_addr() {
        let table_addr: u64 = 0x8000_5000;
        let desc = PtDesc::new()
            .with_valid(true)
            .with_desc_type(true)
            .with_addr_bits(table_addr >> 12);

        assert_eq!(desc.next_table_addr(12), table_addr);
    }

    #[test]
    fn test_pt_desc_access_flag() {
        let desc = PtDesc::new()
            .with_valid(true)
            .with_desc_type(true)
            .with_af(true);
        assert!(desc.af());

        let desc = PtDesc::new()
            .with_valid(true)
            .with_desc_type(true)
            .with_af(false);
        assert!(!desc.af());
    }

    #[test]
    fn test_pt_desc_permissions() {
        // RW_EL1
        let desc = PtDesc::new()
            .with_valid(true)
            .with_desc_type(true)
            .with_ap(ApBits::RW_EL1.0);
        assert_eq!(desc.ap(), ApBits::RW_EL1.0);

        // RO_EL1
        let desc = desc.with_ap(ApBits::RO_EL1.0);
        assert_eq!(desc.ap(), ApBits::RO_EL1.0);
    }

    #[test]
    fn test_ap_bits_write_permission() {
        assert!(ApBits::RW_EL1.allows_write());
        assert!(ApBits::RW_ANY.allows_write());
        assert!(!ApBits::RO_EL1.allows_write());
        assert!(!ApBits::RO_ANY.allows_write());
    }

    #[test]
    fn test_ap_bits_read_permission() {
        // All valid AP values allow reads
        assert!(ApBits::RW_EL1.allows_read());
        assert!(ApBits::RW_ANY.allows_read());
        assert!(ApBits::RO_EL1.allows_read());
        assert!(ApBits::RO_ANY.allows_read());
    }

    #[test]
    fn test_pt_desc_full_roundtrip() {
        let desc = PtDesc::new()
            .with_valid(true)
            .with_desc_type(true)
            .with_attr_index(3)
            .with_ns(true)
            .with_ap(ApBits::RO_ANY.0)
            .with_sh(Shareability::INNER_SHAREABLE.0)
            .with_af(true)
            .with_ng(true)
            .with_addr_bits(0x1234_5000_u64 >> 12)
            .with_pxn(true)
            .with_uxn(true);

        assert!(desc.valid());
        assert!(desc.desc_type());
        assert_eq!(desc.attr_index(), 3);
        assert!(desc.ns());
        assert_eq!(desc.ap(), ApBits::RO_ANY.0);
        assert_eq!(desc.sh(), Shareability::INNER_SHAREABLE.0);
        assert!(desc.af());
        assert!(desc.ng());
        assert_eq!(desc.next_table_addr(12), 0x1234_5000);
        assert!(desc.pxn());
        assert!(desc.uxn());
    }

    #[test]
    fn test_pt_desc_preserves_page_offset() {
        // Verify that the output address does not include sub-page bits
        let page_addr: u64 = 0x8000_3000;
        let desc = PtDesc::new()
            .with_valid(true)
            .with_desc_type(true)
            .with_addr_bits(page_addr >> 12);

        // At L3, the output is the page base
        assert_eq!(desc.output_address_4k(3), page_addr);
        assert_eq!(desc.output_address_4k(3) & 0xFFF, 0);
    }

    #[test]
    fn test_shareability_values() {
        assert_eq!(Shareability::NON_SHAREABLE.0, 0b00);
        assert_eq!(Shareability::OUTER_SHAREABLE.0, 0b10);
        assert_eq!(Shareability::INNER_SHAREABLE.0, 0b11);
    }
}
