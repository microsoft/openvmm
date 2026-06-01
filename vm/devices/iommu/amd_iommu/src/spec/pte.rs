// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! I/O Page Table Entry (PTE/PDE) for the AMD IOMMU.
//!
//! 64-bit entries used in the IOMMU I/O page tables. Based on AMD IOMMU
//! Specification Rev 3.11, §2.2.3, Figures 8–10, Tables 15–18.
//!
//! The AMD IOMMU I/O page table uses a structure similar to x86-64 page tables
//! (9 bits per level, 8 bytes per entry) but differs in field semantics:
//! - `IR` (bit 61) and `IW` (bit 62) replace x86-64 R/W and NX bits.
//! - `NextLevel` (bits 11:9) indicates the next page table level or marks
//!   a leaf entry (NextLevel = 0 or 7).
//! - Permissions are AND-accumulated across all levels including the DTE.
//! - Level skipping is supported when `PDE.NextLevel < (current_level - 1)`.

use bitfield_struct::bitfield;
use inspect::Inspect;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// A 64-bit I/O Page Table Entry (PTE or PDE).
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
#[rustfmt::skip]
pub struct IommuPte {
    /// Present — 1 = this entry is valid.
    pub pr: bool,
    #[bits(4)]
    _ignored1: u64,
    /// Accessed — set by hardware if HD support enabled (not used in emulator).
    pub a: bool,
    /// Dirty — set by hardware if HD support enabled (not used in emulator).
    pub d: bool,
    #[bits(2)]
    _ignored2: u64,
    /// Next level indicator (bits 11:9).
    /// - For PDE (non-leaf): 1–6 = target level.
    /// - For PTE (leaf): 0 or 7 = leaf marker.
    #[bits(3)]
    pub next_level: u8,
    /// Address bits [51:12].
    /// - For PDE: next-level table base address.
    /// - For PTE: physical page frame number.
    #[bits(40)]
    pub address: u64,
    #[bits(5)]
    _reserved: u64,
    /// Page migration state (reserved for our emulator).
    #[bits(2)]
    pub pms: u8,
    /// User bit (ATS attribute, not used).
    pub u: bool,
    /// Force coherent.
    pub fc: bool,
    /// IOMMU Read permission — 1 = reads allowed.
    pub ir: bool,
    /// IOMMU Write permission — 1 = writes allowed.
    pub iw: bool,
    #[bits(1)]
    _ignored3: u64,
}

/// PTE size in bytes.
pub const PTE_SIZE: usize = 8;

/// Number of entries per page table page (4KB / 8 bytes = 512).
pub const ENTRIES_PER_TABLE: usize = 512;

/// Bits per page table level index (log2 of 512 = 9).
pub const BITS_PER_LEVEL: u32 = 9;

/// Page size at level 1 (4KB).
pub const PAGE_SIZE_4K: u64 = 4096;

impl IommuPte {
    /// Check if this entry is present (PR=1).
    pub fn is_present(&self) -> bool {
        self.pr()
    }

    /// Check if this is a leaf entry (PTE, not PDE).
    ///
    /// A leaf entry has NextLevel = 0 or NextLevel = 7.
    pub fn is_leaf(&self) -> bool {
        let nl = self.next_level();
        nl == 0 || nl == 7
    }

    /// Check if this entry has read permission.
    pub fn has_read(&self) -> bool {
        self.ir()
    }

    /// Check if this entry has write permission.
    pub fn has_write(&self) -> bool {
        self.iw()
    }

    /// Get the full physical address from the address field (bits 51:12 shifted left by 12).
    pub fn phys_address(&self) -> u64 {
        self.address() << 12
    }

    /// Compute the page table entry index for a given VA and level.
    ///
    /// Extracts the 9-bit index from `va` at the bit range for `level`:
    /// `(va >> shift) & 0x1FF`, where `shift = 12 + (level - 1) * 9`.
    ///
    /// Level 1: bits 20:12 (shift=12)
    /// Level 2: bits 29:21 (shift=21)
    /// Level 3: bits 38:30 (shift=30)
    /// Level 4: bits 47:39 (shift=39)
    /// Level 5: bits 56:48 (shift=48)
    /// Level 6: bits 63:57 — only 7 bits at this level
    pub fn va_index(va: u64, level: u8) -> usize {
        let shift = 12 + (level as u32 - 1) * BITS_PER_LEVEL;
        ((va >> shift) & 0x1FF) as usize
    }

    /// Compute the page size for a large page at the given level.
    ///
    /// Level 1: 4KB, Level 2: 2MB, Level 3: 1GB, Level 4: 512GB, etc.
    pub fn page_size_at_level(level: u8) -> u64 {
        1u64 << (12 + (level as u32 - 1) * BITS_PER_LEVEL)
    }

    /// Compute the offset mask for a page at the given level.
    ///
    /// This mask extracts the page-offset bits from a VA for a page mapped
    /// at the given level.
    pub fn page_offset_mask(level: u8) -> u64 {
        Self::page_size_at_level(level) - 1
    }

    /// Decode the page size of a mode-7 (NextLevel = 7) large-page leaf PTE.
    ///
    /// AMD IOMMU §2.2.3 ("Default page size" / "level skipping") allows a leaf
    /// PTE to describe a page of arbitrary power-of-two size, encoded inline
    /// in the address field. The trailing 1-bits below the first cleared bit
    /// of the address field indicate the page size:
    ///
    /// ```text
    ///   page_size = 1 << (1 + ffz(raw_pte | 0xFFF))
    /// ```
    ///
    /// where `ffz` finds the index of the first zero bit. (The `| 0xFFF`
    /// forces the non-address low bits to 1 so that `ffz` sees only the
    /// address field's encoded bits.)
    ///
    /// This matches Linux's `PTE_PAGE_SIZE` macro in
    /// `drivers/iommu/amd/amd_iommu_types.h`.
    ///
    /// Returns `None` if the encoding is invalid (no zero bit found in the
    /// address field, or a page size outside the supported range).
    pub fn mode7_page_size(raw_pte: u64) -> Option<u64> {
        // First zero bit, starting from bit 0 (forced to bit 12 or higher
        // by the `| 0xFFF`).
        let first_zero = (raw_pte | 0xFFF).trailing_ones();
        // Hardware addresses are 52-bit (PM_ADDR_MASK covers bits 51:12), so
        // the highest meaningful page-size bit is 51. That yields a max
        // page size of 1 << 52. Anything beyond is malformed.
        if first_zero >= 52 {
            return None;
        }
        Some(1u64 << (first_zero + 1))
    }

    /// Recover the physical page base from a mode-7 leaf PTE.
    ///
    /// `mode7_page_size` decodes the page size from the address field; the
    /// remaining (size-aligned) high bits of the address field form the page
    /// base.
    pub fn mode7_page_base(raw_pte: u64, page_size: u64) -> u64 {
        // `phys_address()` is the 40-bit address field shifted left by 12.
        // Clear the size-encoding low bits to recover the actual base.
        let pte = IommuPte::from_bits(raw_pte);
        pte.phys_address() & !(page_size - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pte_size() {
        assert_eq!(size_of::<IommuPte>(), PTE_SIZE);
        assert_eq!(PTE_SIZE, 8);
    }

    #[test]
    fn test_pte_leaf_detection() {
        // NextLevel = 0 → leaf
        let pte = IommuPte::new().with_pr(true).with_next_level(0);
        assert!(pte.is_leaf());

        // NextLevel = 7 → leaf
        let pte = IommuPte::new().with_pr(true).with_next_level(7);
        assert!(pte.is_leaf());

        // NextLevel = 3 → non-leaf (PDE)
        let pde = IommuPte::new().with_pr(true).with_next_level(3);
        assert!(!pde.is_leaf());
    }

    #[test]
    fn test_pte_present() {
        let pte = IommuPte::new().with_pr(true);
        assert!(pte.is_present());

        let pte = IommuPte::new().with_pr(false);
        assert!(!pte.is_present());
    }

    #[test]
    fn test_pte_permissions() {
        let pte = IommuPte::new().with_ir(true).with_iw(false);
        assert!(pte.has_read());
        assert!(!pte.has_write());

        let pte = IommuPte::new().with_ir(true).with_iw(true);
        assert!(pte.has_read());
        assert!(pte.has_write());
    }

    #[test]
    fn test_pte_address() {
        // Address field = 0x12345 → physical address = 0x12345 << 12 = 0x12345000
        let pte = IommuPte::new().with_address(0x12345);
        assert_eq!(pte.phys_address(), 0x12345000);
    }

    #[test]
    fn test_va_index_level1() {
        // VA = 0x12345678, level 1: bits [20:12] = 0x45678 >> 12 = 0x45 → index = 0x45
        let idx = IommuPte::va_index(0x12345678, 1);
        assert_eq!(idx, (0x12345678u64 >> 12) as usize & 0x1FF);
    }

    #[test]
    fn test_va_index_level4() {
        // VA = 0x0000_7F80_0000_0000, level 4: bits [47:39]
        let va = 0x0000_7F80_0000_0000u64;
        let idx = IommuPte::va_index(va, 4);
        assert_eq!(idx, ((va >> 39) & 0x1FF) as usize);
        assert_eq!(idx, 0xFF);
    }

    #[test]
    fn test_page_size_at_level() {
        assert_eq!(IommuPte::page_size_at_level(1), 4096); // 4KB
        assert_eq!(IommuPte::page_size_at_level(2), 2 * 1024 * 1024); // 2MB
        assert_eq!(IommuPte::page_size_at_level(3), 1024 * 1024 * 1024); // 1GB
        assert_eq!(IommuPte::page_size_at_level(4), 512 * 1024 * 1024 * 1024); // 512GB
    }

    #[test]
    fn test_page_offset_mask() {
        assert_eq!(IommuPte::page_offset_mask(1), 0xFFF); // 4KB - 1
        assert_eq!(IommuPte::page_offset_mask(2), 0x1F_FFFF); // 2MB - 1
        assert_eq!(IommuPte::page_offset_mask(3), 0x3FFF_FFFF); // 1GB - 1
    }

    #[test]
    fn test_pte_roundtrip() {
        let pte = IommuPte::new()
            .with_pr(true)
            .with_next_level(0) // leaf
            .with_address(0x00_DEAD_BEEF) // 40-bit address field
            .with_ir(true)
            .with_iw(true)
            .with_fc(true);
        assert!(pte.is_present());
        assert!(pte.is_leaf());
        assert!(pte.has_read());
        assert!(pte.has_write());
        assert!(pte.fc());
        assert_eq!(pte.address(), 0x00_DEAD_BEEF);
    }

    #[test]
    fn test_pde_roundtrip() {
        // PDE pointing to level 2, next-table at 0x100000
        let pde = IommuPte::new()
            .with_pr(true)
            .with_next_level(2)
            .with_address(0x100) // 0x100 << 12 = 0x100000
            .with_ir(true)
            .with_iw(true);
        assert!(pde.is_present());
        assert!(!pde.is_leaf());
        assert_eq!(pde.next_level(), 2);
        assert_eq!(pde.phys_address(), 0x100000);
    }

    #[test]
    fn test_entries_per_table() {
        assert_eq!(ENTRIES_PER_TABLE, 512);
        assert_eq!(ENTRIES_PER_TABLE * PTE_SIZE, 4096); // one 4KB page
    }

    /// Mirror Linux's `PAGE_SIZE_PTE` macro from `amd_iommu_types.h`:
    ///
    /// ```c
    /// #define PAGE_SIZE_PTE(address, pagesize) \
    ///         (((address) | ((pagesize) - 1)) & (~((pagesize) >> 1)) & PM_ADDR_MASK)
    /// ```
    fn encode_mode7_address_field(paddr: u64, pgsize: u64) -> u64 {
        const PM_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
        ((paddr | (pgsize - 1)) & !(pgsize >> 1)) & PM_ADDR_MASK
    }

    #[test]
    fn test_mode7_page_size_roundtrip_small() {
        // 8 KiB page at paddr 0x80000.
        let pgsize = 0x2000u64;
        let paddr = 0x80000u64;
        let addr_field = encode_mode7_address_field(paddr, pgsize);
        let raw = IommuPte::new()
            .with_pr(true)
            .with_next_level(7)
            .with_address(addr_field >> 12)
            .with_ir(true)
            .with_iw(true)
            .into_bits();

        assert_eq!(IommuPte::mode7_page_size(raw), Some(pgsize));
        assert_eq!(IommuPte::mode7_page_base(raw, pgsize), paddr);
    }

    #[test]
    fn test_mode7_page_size_roundtrip_16k() {
        // 16 KiB page at paddr 0x80000.
        let pgsize = 0x4000u64;
        let paddr = 0x80000u64;
        let addr_field = encode_mode7_address_field(paddr, pgsize);
        let raw = IommuPte::new()
            .with_pr(true)
            .with_next_level(7)
            .with_address(addr_field >> 12)
            .into_bits();

        assert_eq!(IommuPte::mode7_page_size(raw), Some(pgsize));
        assert_eq!(IommuPte::mode7_page_base(raw, pgsize), paddr);
    }

    #[test]
    fn test_mode7_page_size_roundtrip_64k() {
        // 64 KiB page at paddr 0x1_0000.
        let pgsize = 0x10000u64;
        let paddr = 0x10000u64;
        let addr_field = encode_mode7_address_field(paddr, pgsize);
        let raw = IommuPte::new()
            .with_pr(true)
            .with_next_level(7)
            .with_address(addr_field >> 12)
            .into_bits();

        assert_eq!(IommuPte::mode7_page_size(raw), Some(pgsize));
        assert_eq!(IommuPte::mode7_page_base(raw, pgsize), paddr);
    }

    #[test]
    fn test_mode7_page_size_roundtrip_4m() {
        // 4 MiB page at paddr 0x40_0000.
        let pgsize = 0x40_0000u64;
        let paddr = 0x40_0000u64;
        let addr_field = encode_mode7_address_field(paddr, pgsize);
        let raw = IommuPte::new()
            .with_pr(true)
            .with_next_level(7)
            .with_address(addr_field >> 12)
            .into_bits();

        assert_eq!(IommuPte::mode7_page_size(raw), Some(pgsize));
        assert_eq!(IommuPte::mode7_page_base(raw, pgsize), paddr);
    }

    #[test]
    fn test_mode7_page_size_with_flag_bits() {
        // Verify flag bits (PR, IR, IW, FC) don't affect size decoding.
        let pgsize = 0x10000u64;
        let paddr = 0x20_0000u64;
        let addr_field = encode_mode7_address_field(paddr, pgsize);
        let raw = IommuPte::new()
            .with_pr(true)
            .with_next_level(7)
            .with_address(addr_field >> 12)
            .with_ir(true)
            .with_iw(true)
            .with_fc(true)
            .into_bits();

        assert_eq!(IommuPte::mode7_page_size(raw), Some(pgsize));
        assert_eq!(IommuPte::mode7_page_base(raw, pgsize), paddr);
    }
}
