// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Methods to construct page tables on x64.

use crate::IdentityMapSize;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

const X64_PTE_PRESENT: u64 = 1;
const X64_PTE_READ_WRITE: u64 = 1 << 1;
const X64_PTE_ACCESSED: u64 = 1 << 5;
const X64_PTE_DIRTY: u64 = 1 << 6;
const X64_PTE_LARGE_PAGE: u64 = 1 << 7;

const PAGE_TABLE_ENTRY_COUNT: usize = 512;

const X64_PAGE_SHIFT: u64 = 12;
const X64_PTE_BITS: u64 = 9;

/// Number of bytes in a page for X64.
pub const X64_PAGE_SIZE: u64 = 4096;

/// Number of bytes in a large page for X64.
pub const X64_LARGE_PAGE_SIZE: u64 = 0x200000;

/// Number of bytes in a 1GB page for X64.
pub const X64_1GB_PAGE_SIZE: u64 = 0x40000000;

#[derive(Copy, Clone, PartialEq, Eq, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(transparent)]
pub struct PageTableEntry {
    pub(crate) entry: u64,
}

impl std::fmt::Debug for PageTableEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageTableEntry")
            .field("entry", &self.entry)
            .field("is_present", &self.is_present())
            .field("is_large_page", &self.is_large_page())
            .field("gpa", &self.gpa())
            .finish()
    }
}

#[derive(Debug, Copy, Clone)]
pub enum PageTableEntryType {
    Leaf1GbPage(u64),
    Leaf2MbPage(u64),
    Leaf4kPage(u64),
    Pde(u64),
}

pub trait PteOps {
    fn get_addr_mask(&self) -> u64;
    fn get_confidential_mask(&self) -> u64;

    fn build_pte(entry_type: PageTableEntryType) -> PageTableEntry {
        let mut entry: u64 = X64_PTE_PRESENT | X64_PTE_ACCESSED | X64_PTE_READ_WRITE;

        match entry_type {
            PageTableEntryType::Leaf1GbPage(address) => {
                // Must be 1GB aligned.
                assert!(address % X64_1GB_PAGE_SIZE == 0);
                entry |= address;
                entry |= X64_PTE_LARGE_PAGE | X64_PTE_DIRTY;
            }
            PageTableEntryType::Leaf2MbPage(address) => {
                // Leaf entry, set like UEFI does for 2MB pages. Must be 2MB aligned.
                assert!(address % X64_LARGE_PAGE_SIZE == 0);
                entry |= address;
                entry |= X64_PTE_LARGE_PAGE | X64_PTE_DIRTY;
            }
            PageTableEntryType::Leaf4kPage(address) => {
                // Must be 4K aligned.
                assert!(address % X64_PAGE_SIZE == 0);
                entry |= address;
                entry |= X64_PTE_DIRTY;
            }
            PageTableEntryType::Pde(address) => {
                // Points to another pagetable.
                assert!(address % X64_PAGE_SIZE == 0);
                entry |= address;
            }
        }

        PageTableEntry { entry }
    }

    fn is_pte_present(pte: &PageTableEntry) -> bool {
        pte.is_present()
    }

    fn is_pte_large_page(pte: &PageTableEntry) -> bool {
        pte.is_large_page()
    }

    fn get_gpa_from_pte(&self, pte: &PageTableEntry) -> Option<u64> {
        if pte.is_present() {
            Some(self.get_addr_from_pte(pte))
        } else {
            None
        }
    }

    fn get_addr_from_pte(&self, pte: &PageTableEntry) -> u64 {
        pte.entry & self.get_addr_mask()
    }

    fn set_addr_in_pte(&self, pte: &mut PageTableEntry, address: u64) {
        let mask = self.get_addr_mask();
        pte.entry = (pte.entry & !mask) | (address & mask);
    }

    fn set_pte_confidentiality(&self, pte: &mut PageTableEntry, confidential: bool) {
        let mask = self.get_confidential_mask();
        if confidential {
            pte.entry |= mask;
        } else {
            pte.entry &= !mask;
        }
    }
}

impl PageTableEntry {
    const VALID_BITS: u64 = 0x000f_ffff_ffff_f000;

    /// Set an AMD64 PDE to either represent a leaf 2MB page or PDE.
    /// This sets the PTE to preset, accessed, dirty, read write execute.
    pub fn set_entry(&mut self, entry_type: PageTableEntryType) {
        self.entry = X64_PTE_PRESENT | X64_PTE_ACCESSED | X64_PTE_READ_WRITE;

        match entry_type {
            PageTableEntryType::Leaf1GbPage(address) => {
                // Must be 1GB aligned.
                assert!(address % X64_1GB_PAGE_SIZE == 0);
                self.entry |= address;
                self.entry |= X64_PTE_LARGE_PAGE | X64_PTE_DIRTY;
            }
            PageTableEntryType::Leaf2MbPage(address) => {
                // Leaf entry, set like UEFI does for 2MB pages. Must be 2MB aligned.
                assert!(address % X64_LARGE_PAGE_SIZE == 0);
                self.entry |= address;
                self.entry |= X64_PTE_LARGE_PAGE | X64_PTE_DIRTY;
            }
            PageTableEntryType::Leaf4kPage(address) => {
                // Must be 4K aligned.
                assert!(address % X64_PAGE_SIZE == 0);
                self.entry |= address;
                self.entry |= X64_PTE_DIRTY;
            }
            PageTableEntryType::Pde(address) => {
                // Points to another pagetable.
                assert!(address % X64_PAGE_SIZE == 0);
                self.entry |= address;
            }
        }
    }

    pub fn is_present(&self) -> bool {
        self.entry & X64_PTE_PRESENT == X64_PTE_PRESENT
    }

    pub fn is_large_page(&self) -> bool {
        self.entry & X64_PTE_LARGE_PAGE == X64_PTE_LARGE_PAGE
    }

    pub fn gpa(&self) -> Option<u64> {
        if self.is_present() {
            // bits 51 to 12 describe the gpa of the next page table
            Some(self.entry & Self::VALID_BITS)
        } else {
            None
        }
    }

    pub fn set_addr(&mut self, addr: u64) {
        assert!(addr & !Self::VALID_BITS == 0);

        // clear addr bits, set new addr
        self.entry &= !Self::VALID_BITS;
        self.entry |= addr;
    }

    pub fn get_addr(&self) -> u64 {
        self.entry & Self::VALID_BITS
    }

    pub fn clear(&mut self) {
        self.entry = 0;
    }
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Eq, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PageTable {
    entries: [PageTableEntry; PAGE_TABLE_ENTRY_COUNT],
}

impl PageTable {
    // fn iter(&self) -> impl Iterator<Item = &PageTableEntry> {
    //     self.entries.iter()
    // }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut PageTableEntry> {
        self.entries.iter_mut()
    }

    /// Treat this page table as a page table of a given level, and locate the entry corresponding to a va.
    pub fn entry(&mut self, gva: u64, level: u8) -> &mut PageTableEntry {
        let index = get_amd64_pte_index(gva, level as u64) as usize;
        &mut self.entries[index]
    }
}

impl std::ops::Index<usize> for PageTable {
    type Output = PageTableEntry;

    fn index(&self, index: usize) -> &Self::Output {
        &self.entries[index]
    }
}

impl std::ops::IndexMut<usize> for PageTable {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.entries[index]
    }
}

/// Get an AMD64 PTE index based on page table level.
pub fn get_amd64_pte_index(gva: u64, page_map_level: u64) -> u64 {
    let index = gva >> (X64_PAGE_SHIFT + page_map_level * X64_PTE_BITS);
    index & ((1 << X64_PTE_BITS) - 1)
}

/// Calculate the number of PDE page tables required to identity map a given gpa and size.
pub fn calculate_pde_table_count(start_gpa: u64, size: u64) -> u64 {
    let mut count = 0;

    // Determine the number of bytes from start up to the next 1GB aligned
    let start_aligned_up = align_up_to_1_gb_page_size(start_gpa);
    let end_gpa = start_gpa + size;
    let end_aligned_down = (end_gpa / X64_1GB_PAGE_SIZE) * X64_1GB_PAGE_SIZE;

    // Ranges sized less than 1GB are treated differently.
    if size < X64_1GB_PAGE_SIZE {
        // A range either takes one or two pages depending on if it crosses a 1GB boundary.
        if end_gpa > end_aligned_down && start_gpa < end_aligned_down {
            count = 2;
        } else {
            count = 1;
        }
    } else {
        // Count the first unaligned start up to an aligned 1GB range.
        if start_gpa != start_aligned_up {
            count += 1;
        }

        // Add the inner ranges that are 1GB aligned.
        if end_aligned_down > start_aligned_up {
            count += (end_aligned_down - start_aligned_up) / X64_1GB_PAGE_SIZE;
        }

        // Add any unaligned end range.
        if end_gpa > end_aligned_down {
            count += 1;
        }
    }

    count
}

#[derive(Debug, Clone)]
pub struct PageTableBuilder {
    page_table_gpa: u64,
    start_gpa: u64,
    size: u64,
    local_map: Option<(u64, u64)>,
    confidential_bit: Option<u32>,
    map_reset_vector: bool,
}

impl PteOps for PageTableBuilder {
    fn get_addr_mask(&self) -> u64 {
        const ALL_ADDR_BITS: u64 = 0x000f_ffff_ffff_f000;
        ALL_ADDR_BITS & !self.get_confidential_mask()
    }

    fn get_confidential_mask(&self) -> u64 {
        if let Some(confidential_bit) = self.confidential_bit {
            1u64 << confidential_bit
        } else {
            0
        }
    }
}

impl PageTableBuilder {
    pub fn new(page_table_gpa: u64) -> Self {
        PageTableBuilder {
            page_table_gpa,
            start_gpa: 0,
            size: 0,
            local_map: None,
            confidential_bit: None,
            map_reset_vector: false,
        }
    }

    pub fn with_mapped_region(mut self, start_gpa: u64, size: u64) -> Self {
        self.start_gpa = start_gpa;
        self.size = size;
        self
    }

    pub fn with_local_map(mut self, start_va: u64, size: u64) -> Self {
        self.local_map = Some((start_va, size));
        self
    }

    pub fn with_confidential_bit(mut self, bit_position: u32) -> Self {
        self.confidential_bit = Some(bit_position);
        self
    }

    /// Map the reset vector at page 0xFFFFF with a single page.
    pub fn with_reset_vector(mut self, map_reset_vector: bool) -> Self {
        self.map_reset_vector = map_reset_vector;
        self
    }

    /// Build a set of X64 page tables identity mapping the given region. `size` must be less than 512GB.
    /// This creates up to 3+N page tables: 1 PML4E and up to 2 PDPTE tables, and N page tables counted at 1 per GB of size,
    /// for 2MB mappings.
    pub fn build(self) -> Vec<u8> {
        const SIZE_512_GB: u64 = 0x8000000000;

        if self.size == 0 {
            panic!("size not set");
        }

        if self.size > SIZE_512_GB {
            panic!("more than 512 gb size not supported");
        }

        if self.size % X64_LARGE_PAGE_SIZE != 0 {
            panic!("size not 2mb aligned");
        }

        // start_gpa and size must be 2MB aligned.
        if self.start_gpa % X64_LARGE_PAGE_SIZE != 0 {
            panic!("start_gpa not 2mb aligned");
        }

        let start_va = self.start_gpa;
        let end_va = start_va + self.size;
        let page_table_gpa = self.page_table_gpa;

        if let Some((local_map_start, local_map_size)) = self.local_map {
            if local_map_start % X64_LARGE_PAGE_SIZE != 0 {
                panic!("local map address not 2 mb aligned");
            }

            if local_map_size % X64_LARGE_PAGE_SIZE != 0 {
                panic!("local map size not 2 mb aligned");
            }

            if local_map_size == 0 {
                panic!("local map size cannot be 0");
            }

            let local_map_end = local_map_start + local_map_size;
            if local_map_end > start_va && local_map_start < end_va {
                panic!("local map overlaps with mapped region");
            }
        }

        // Allocate single PML4E page table.
        let mut page_table: Vec<PageTable> = Vec::new();
        page_table.push(PageTable::new_zeroed());
        let pml4_table_index = 0;
        let confidential = self.confidential_bit.is_some();

        let mut link_tables = |start_va: u64, end_va: u64, use_large_pages: bool| {
            let mut current_va = start_va;
            while current_va < end_va {
                tracing::trace!(current_va);

                let pdpte_table_index = {
                    let next_index = page_table.len();
                    let pml4_entry = page_table[pml4_table_index].entry(current_va, 3);
                    if !pml4_entry.is_present() {
                        // Allocate and link PDPTE table.
                        let output_address = page_table_gpa + next_index as u64 * X64_PAGE_SIZE;
                        let mut new_entry =
                            Self::build_pte(PageTableEntryType::Pde(output_address));
                        self.set_pte_confidentiality(&mut new_entry, confidential);
                        *pml4_entry = new_entry;
                        page_table.push(PageTable::new_zeroed());

                        next_index
                    } else {
                        ((self.get_addr_from_pte(pml4_entry) - page_table_gpa) / X64_PAGE_SIZE)
                            .try_into()
                            .expect("Valid page table index")
                    }
                };

                tracing::trace!(pdpte_table_index);

                let pde_table_index = {
                    let next_index = page_table.len();
                    let pdpte_entry = page_table[pdpte_table_index].entry(current_va, 2);
                    if !pdpte_entry.is_present() {
                        // Allocate and link PDE table.
                        let output_address = page_table_gpa + next_index as u64 * X64_PAGE_SIZE;
                        let mut new_entry =
                            Self::build_pte(PageTableEntryType::Pde(output_address));
                        self.set_pte_confidentiality(&mut new_entry, confidential);
                        *pdpte_entry = new_entry;
                        page_table.push(PageTable::new_zeroed());

                        next_index
                    } else {
                        ((self.get_addr_from_pte(pdpte_entry) - page_table_gpa) / X64_PAGE_SIZE)
                            .try_into()
                            .expect("Valid page table index")
                    }
                };

                tracing::trace!(pde_table_index);

                let next_index = page_table.len();
                let pde_entry = page_table[pde_table_index].entry(current_va, 1);
                assert!(!pde_entry.is_present());

                if use_large_pages {
                    assert!(!pde_entry.is_present());

                    let mut new_entry =
                        Self::build_pte(PageTableEntryType::Leaf2MbPage(current_va));
                    self.set_pte_confidentiality(&mut new_entry, confidential);
                    *pde_entry = new_entry;
                    current_va += X64_LARGE_PAGE_SIZE;
                } else {
                    let pt_table_index = if !pde_entry.is_present() {
                        // Allocate and link page table.
                        let output_address = page_table_gpa + next_index as u64 * X64_PAGE_SIZE;
                        let mut new_entry =
                            Self::build_pte(PageTableEntryType::Pde(output_address));
                        self.set_pte_confidentiality(&mut new_entry, confidential);
                        *pde_entry = new_entry;
                        page_table.push(PageTable::new_zeroed());

                        next_index
                    } else {
                        ((self.get_addr_from_pte(pde_entry) - page_table_gpa) / X64_PAGE_SIZE)
                            .try_into()
                            .expect("Valid page table index")
                    };

                    tracing::trace!(pt_table_index);

                    let pt_entry = page_table[pt_table_index].entry(current_va, 0);
                    let mut new_entry = Self::build_pte(PageTableEntryType::Leaf4kPage(current_va));
                    self.set_pte_confidentiality(&mut new_entry, confidential);
                    *pt_entry = new_entry;
                    current_va += X64_PAGE_SIZE;
                }
            }
        };

        link_tables(start_va, end_va, true);

        // Create local map area if present.
        if let Some((local_map_start, local_map_size)) = self.local_map {
            link_tables(local_map_start, local_map_start + local_map_size, true);
        }

        if self.map_reset_vector {
            // Map the reset vector pfn of 0xFFFFF
            tracing::trace!("identity mapping reset page 0xFFFFF");
            let reset_vector_addr = 0xFFFFF * X64_PAGE_SIZE;
            link_tables(reset_vector_addr, reset_vector_addr + X64_PAGE_SIZE, false);
        }

        // Flatten page table vec into u8 vec
        flatten_page_table(page_table)
    }
}

/// Build a set of X64 page tables identity mapping the bottom address
/// space with an optional address bias.
///
/// An optional PML4E entry may be linked, with arguments being (link_target_gpa, linkage_gpa).
/// link_target_gpa represents the GPA of the PML4E to link into the built page table.
/// linkage_gpa represents the GPA at which the linked PML4E should be linked.
pub fn build_page_tables_64(
    page_table_gpa: u64,
    address_bias: u64,
    identity_map_size: IdentityMapSize,
    pml4e_link: Option<(u64, u64)>,
) -> Vec<u8> {
    // Allocate page tables. There are up to 6 total page tables:
    //      1 PML4E (Level 4) (omitted if the address bias is non-zero)
    //      1 PDPTE (Level 3)
    //      4 or 8 PDE tables (Level 2)
    // Note that there are no level 1 page tables, as 2MB pages are used.
    let leaf_page_table_count = match identity_map_size {
        IdentityMapSize::Size4Gb => 4,
        IdentityMapSize::Size8Gb => 8,
    };
    let page_table_count = leaf_page_table_count + if address_bias == 0 { 2 } else { 1 };
    let mut page_table: Vec<PageTable> = vec![PageTable::new_zeroed(); page_table_count];
    let mut page_table_allocator = page_table.iter_mut().enumerate();

    // Allocate single PDPTE table.
    let pdpte_table = if address_bias == 0 {
        // Allocate single PML4E page table.
        let (_, pml4e_table) = page_table_allocator
            .next()
            .expect("pagetable should always be available, code bug if not");

        // PDPTE table is the next pagetable.
        let (pdpte_table_index, pdpte_table) = page_table_allocator
            .next()
            .expect("pagetable should always be available, code bug if not");

        // Set PML4E entry linking PML4E to PDPTE.
        let output_address = page_table_gpa + pdpte_table_index as u64 * X64_PAGE_SIZE;
        pml4e_table[0].set_entry(PageTableEntryType::Pde(output_address));

        // Set PML4E entry to link the additional entry if specified.
        if let Some((link_target_gpa, linkage_gpa)) = pml4e_link {
            assert!((linkage_gpa & 0x7FFFFFFFFF) == 0);
            pml4e_table[linkage_gpa as usize >> 39]
                .set_entry(PageTableEntryType::Pde(link_target_gpa));
        }

        pdpte_table
    } else {
        // PDPTE table is the first table, if no PML4E.
        page_table_allocator
            .next()
            .expect("pagetable should always be available, code bug if not")
            .1
    };

    // Build PDEs that point to 2 MB pages.
    let top_address = match identity_map_size {
        IdentityMapSize::Size4Gb => 0x100000000u64,
        IdentityMapSize::Size8Gb => 0x200000000u64,
    };
    let mut current_va = 0;

    while current_va < top_address {
        // Allocate a new PDE table
        let (pde_table_index, pde_table) = page_table_allocator
            .next()
            .expect("pagetable should always be available, code bug if not");

        // Link PDPTE table to PDE table (L3 to L2)
        let pdpte_index = get_amd64_pte_index(current_va, 2);
        let output_address = page_table_gpa + pde_table_index as u64 * X64_PAGE_SIZE;
        let pdpte_entry = &mut pdpte_table[pdpte_index as usize];
        assert!(!pdpte_entry.is_present());
        pdpte_entry.set_entry(PageTableEntryType::Pde(output_address));

        // Set all 2MB entries in this PDE table.
        for entry in pde_table.iter_mut() {
            entry.set_entry(PageTableEntryType::Leaf2MbPage(current_va + address_bias));
            current_va += X64_LARGE_PAGE_SIZE;
        }
    }

    // All pagetables should be used, code bug if not.
    assert!(page_table_allocator.next().is_none());

    // Flatten page table vec into u8 vec
    flatten_page_table(page_table)
}

/// Align an address up to the start of the next page.
pub fn align_up_to_page_size(address: u64) -> u64 {
    (address + X64_PAGE_SIZE - 1) & !(X64_PAGE_SIZE - 1)
}

/// Align an address up to the start of the next large (2MB) page.
pub fn align_up_to_large_page_size(address: u64) -> u64 {
    (address + X64_LARGE_PAGE_SIZE - 1) & !(X64_LARGE_PAGE_SIZE - 1)
}

/// Align an address up to the start of the next 1GB page.
pub fn align_up_to_1_gb_page_size(address: u64) -> u64 {
    (address + X64_1GB_PAGE_SIZE - 1) & !(X64_1GB_PAGE_SIZE - 1)
}

fn flatten_page_table(page_table: Vec<PageTable>) -> Vec<u8> {
    let mut flat_tables = Vec::with_capacity(page_table.len() * X64_PAGE_SIZE as usize);
    for table in page_table {
        flat_tables.extend_from_slice(table.as_bytes());
    }

    flat_tables
}

#[cfg(test)]
mod tests {
    use super::align_up_to_large_page_size;
    use super::align_up_to_page_size;
    use super::calculate_pde_table_count;
    use super::X64_1GB_PAGE_SIZE;

    #[test]
    fn test_align_up() {
        assert_eq!(align_up_to_page_size(4096), 4096);
        assert_eq!(align_up_to_page_size(4095), 4096);
        assert_eq!(align_up_to_page_size(4097), 8192);
    }

    #[test]
    fn test_large_align_up() {
        assert_eq!(align_up_to_large_page_size(0), 0);
        assert_eq!(align_up_to_large_page_size(4096), 0x200000);
        assert_eq!(align_up_to_large_page_size(0x200000), 0x200000);
        assert_eq!(align_up_to_large_page_size(0x200001), 0x400000);
    }

    #[test]
    fn test_pde_size_calc() {
        assert_eq!(calculate_pde_table_count(0, 512), 1);
        assert_eq!(calculate_pde_table_count(0, 1024 * 1024), 1);
        assert_eq!(calculate_pde_table_count(512, 1024 * 1024), 1);
        assert_eq!(calculate_pde_table_count(X64_1GB_PAGE_SIZE - 512, 1024), 2);
        assert_eq!(calculate_pde_table_count(X64_1GB_PAGE_SIZE - 512, 512), 1);
        assert_eq!(calculate_pde_table_count(0, X64_1GB_PAGE_SIZE), 1);
        assert_eq!(calculate_pde_table_count(0, X64_1GB_PAGE_SIZE + 1), 2);
        assert_eq!(calculate_pde_table_count(1, X64_1GB_PAGE_SIZE + 1), 2);
        assert_eq!(calculate_pde_table_count(512, X64_1GB_PAGE_SIZE * 2), 3);

        assert_eq!(calculate_pde_table_count(0, X64_1GB_PAGE_SIZE * 3), 3);
        assert_eq!(
            calculate_pde_table_count(X64_1GB_PAGE_SIZE, X64_1GB_PAGE_SIZE * 3),
            3
        );
    }
}
