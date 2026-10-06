// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Second-level page table entry types for the Intel VT-d IOMMU.
//!
//! Based on Intel VT-d Specification Rev 4.1, §9.8. The second-level page
//! table format is EPT-like: 64-bit entries, 9 bits per level, with R/W
//! permission bits in bits 1:0 (bit 2 is ignored) and a PS bit at bit 7 for large
//! pages at levels 2 (2MB) and 3 (1GB).
//! Accessed (bit 8) and leaf dirty (bit 9) flags are ignored unless SSADE
//! is enabled in the referencing scalable-mode PASID-table entry (§3.7.2).
//!
//! Page table level parameters:
//! - Level 4: VA\[47:39\], 9 bits → 512GB region
//! - Level 3: VA\[38:30\], 9 bits → 1GB page (if PS=1)
//! - Level 2: VA\[29:21\], 9 bits → 2MB page (if PS=1)
//! - Level 1: VA\[20:12\], 9 bits → 4KB page

use bitfield_struct::bitfield;
use inspect::Inspect;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// A 64-bit second-level page table entry (PTE or PDE).
///
/// EPT-like format used by Intel VT-d for DMA address translation.
/// Permissions (R/W) are AND-accumulated across all levels per §3.7.1.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
#[rustfmt::skip]
pub struct SlPte {
    /// Read permission — 1 = DMA reads allowed through this entry.
    pub r: bool,
    /// Write permission — 1 = DMA writes allowed through this entry.
    pub w: bool,
    /// EPT execute bit; ignored in VT-d second-stage entries.
    pub x: bool,
    #[bits(4)]
    _ignored1: u64,
    /// Page Size — 1 = this is a leaf entry for a large page.
    /// Valid at level 3 (1GB) and level 2 (2MB). Must be 0 at level 4.
    pub ps: bool,
    /// Accessed (bit 8); ignored when SSADE is clear.
    pub a: bool,
    /// Dirty (bit 9); ignored in non-leaf entries or when SSADE is clear.
    pub d: bool,
    #[bits(1)]
    _ignored2: u64,
    /// Snoop behavior (bit 11, ignored by emulator).
    pub snp: bool,
    /// Address bits [51:12] — next-level table or page frame address.
    #[bits(40)]
    pub address: u64,
    #[bits(10)]
    _ignored3: u64,
    #[bits(1)]
    _reserved: u64,
    #[bits(1)]
    _ignored4: u64,
}

/// Number of entries per page table page (4KB / 8 bytes = 512).
pub const ENTRIES_PER_TABLE: usize = 512;

/// Bits per page table level index (log2 of 512 = 9).
pub const BITS_PER_LEVEL: u32 = 9;

/// Page size at level 1 (4KB).
pub const PAGE_SIZE_4K: u64 = 4096;

/// Page size at level 2 (2MB).
pub const PAGE_SIZE_2M: u64 = 1 << 21;

/// Page size at level 3 (1GB).
pub const PAGE_SIZE_1G: u64 = 1 << 30;

impl SlPte {
    /// Check if this entry is present (R or W is set).
    ///
    /// For DMA remapping, an entry is present if any of R or W is set.
    /// The X bit is ignored for second-level entries.
    pub fn is_present(&self) -> bool {
        self.r() || self.w()
    }

    /// Get the full physical address from the address field (bits 51:12 shifted).
    pub fn phys_address(&self) -> u64 {
        self.address() << 12
    }

    /// Compute the page table entry index for a given IOVA and level.
    ///
    /// Extracts the 9-bit index from `iova` at the bit range for `level`:
    /// `(iova >> shift) & 0x1FF`, where `shift = 12 + (level - 1) * 9`.
    ///
    /// Level 1: bits 20:12 (shift=12)
    /// Level 2: bits 29:21 (shift=21)
    /// Level 3: bits 38:30 (shift=30)
    /// Level 4: bits 47:39 (shift=39)
    pub fn iova_index(iova: u64, level: u8) -> usize {
        let shift = 12 + (level as u32 - 1) * BITS_PER_LEVEL;
        ((iova >> shift) & 0x1FF) as usize
    }

    /// Get the page size at a given level (when PS=1 for large pages).
    ///
    /// Level 1: 4KB, Level 2: 2MB, Level 3: 1GB.
    pub fn page_size_at_level(level: u8) -> u64 {
        1u64 << (12 + (level as u32 - 1) * BITS_PER_LEVEL)
    }

    /// Compute the GPA for a large page mapping at the given level.
    ///
    /// Masks the page frame address to the level alignment and adds the
    /// offset from the IOVA within the page.
    pub fn large_page_gpa(&self, iova: u64, level: u8) -> u64 {
        let page_size = Self::page_size_at_level(level);
        let mask = !(page_size - 1);
        (self.phys_address() & mask) | (iova & (page_size - 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn test_accessed_dirty_literal_layout() {
        assert_eq!(size_of::<SlPte>(), 8);
        assert_eq!(align_of::<SlPte>(), 8);
        for (raw, accessed, dirty) in [
            (0x100, true, false),
            (0x200, false, true),
            (0x400, false, false),
        ] {
            let pte = SlPte::from(raw);
            assert_eq!(pte.a(), accessed);
            assert_eq!(pte.d(), dirty);
            assert!(!pte.is_present());
        }
        assert_eq!(SlPte::new().with_a(true).into_bits(), 1 << 8);
        assert_eq!(SlPte::new().with_d(true).into_bits(), 1 << 9);
        assert_eq!(
            SlPte::new().with_a(true).with_d(true).as_bytes(),
            &[0, 3, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn test_accessed_dirty_preserve_other_bits() {
        // Include every unrelated bit, including ignored and reserved bits.
        for bit in 0..64 {
            for raw in [1u64 << bit, !(1u64 << bit)] {
                for accessed in [false, true] {
                    for dirty in [false, true] {
                        let pte = SlPte::from(raw).with_a(accessed).with_d(dirty);
                        assert_eq!(
                            pte.into_bits(),
                            (raw & !0x300) | ((accessed as u64) << 8) | ((dirty as u64) << 9)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_pte_present_requires_r_or_w() {
        assert!(SlPte::new().with_r(true).is_present());
        assert!(SlPte::new().with_w(true).is_present());
        assert!(!SlPte::new().is_present());
        // X=1 alone does NOT make it present for DMA
        assert!(!SlPte::new().with_x(true).is_present());
    }

    #[test]
    fn test_iova_index() {
        assert_eq!(SlPte::iova_index(0x7F80_0000_0000, 4), 0xFF);
    }

    #[test]
    fn test_page_size_at_level() {
        assert_eq!(SlPte::page_size_at_level(1), PAGE_SIZE_4K);
        assert_eq!(SlPte::page_size_at_level(2), PAGE_SIZE_2M);
        assert_eq!(SlPte::page_size_at_level(3), PAGE_SIZE_1G);
    }

    #[test]
    fn test_large_page_gpa() {
        let pte = SlPte::new()
            .with_r(true)
            .with_w(true)
            .with_ps(true)
            .with_address(0x40000); // 0x40000 << 12 = 0x4000_0000
        // 1GB page: base 0x4000_0000, offset 0x1234
        assert_eq!(pte.large_page_gpa(0x4000_1234, 3), 0x4000_1234);
        // 2MB page
        let pte2 = SlPte::new()
            .with_r(true)
            .with_w(true)
            .with_ps(true)
            .with_address(0x200); // 0x200 << 12 = 0x20_0000
        assert_eq!(pte2.large_page_gpa(0x0020_1000, 2), 0x0020_1000);
    }
}
