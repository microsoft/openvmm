// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Methods to construct page tables on x64.

use crate::Error;
use crate::IdentityMapSize;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

const X64_PTE_PRESENT: u64 = 1;
const X64_PTE_READ_WRITE: u64 = 1 << 1;
const X64_PTE_ACCESSED: u64 = 1 << 5;
const X64_PTE_DIRTY: u64 = 1 << 6;
const X64_PTE_LARGE_PAGE: u64 = 1 << 7;

const PAGE_TABLE_ENTRY_COUNT: usize = 512;
const PAGE_TABLE_ENTRY_SIZE: usize = 8;

const X64_PAGE_SHIFT: u64 = 12;
const X64_PTE_BITS: u64 = 9;

/// Number of bytes in a page for X64.
pub const X64_PAGE_SIZE: u64 = 4096;

/// Number of bytes in a large page for X64.
pub const X64_LARGE_PAGE_SIZE: u64 = 0x200000;

/// Number of bytes in a 1GB page for X64.
pub const X64_1GB_PAGE_SIZE: u64 = 0x40000000;

/// Maximum number of page tables created for an x64 identity map
pub const PAGE_TABLE_MAX_COUNT: usize = 20;

static_assertions::const_assert_eq!(
    PAGE_TABLE_ENTRY_SIZE * PAGE_TABLE_ENTRY_COUNT,
    X64_PAGE_SIZE as usize
);
const PAGE_TABLE_SIZE: usize = PAGE_TABLE_ENTRY_COUNT * PAGE_TABLE_ENTRY_SIZE;

/// Maximum number of bytes needed to store an x64 identity map
pub const PAGE_TABLE_MAX_BYTES: usize = PAGE_TABLE_MAX_COUNT * X64_PAGE_SIZE as usize;

#[derive(Copy, Clone, PartialEq, Eq, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(transparent)]
pub struct PageTableEntry {
    pub(crate) entry: u64,
}

#[derive(Copy, Clone, Debug)]
pub struct MappedRange {
    start: u64,
    end: u64,
    permissions: Option<u64>,
}

impl MappedRange {
    pub fn new(start: u64, end: u64) -> Self {
        Self {
            start,
            end,
            permissions: Some(X64_PTE_READ_WRITE),
        }
    }

    pub fn start(&self) -> u64 {
        self.start
    }

    pub fn end(&self) -> u64 {
        self.end
    }

    // Consumes a mapped range, and returns the range as read-only
    pub fn read_only(mut self) -> Self {
        self.permissions = None;
        self
    }
}

impl core::fmt::Debug for PageTableEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
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

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum EntryLevel {
    PML4 = 3,
    PDPT = 2,
    PD = 1,
    PT = 0,
}

impl EntryLevel {
    pub fn mapping_size(self) -> u64 {
        match self {
            Self::PML4 => X64_1GB_PAGE_SIZE * 512,
            Self::PDPT => X64_1GB_PAGE_SIZE,
            Self::PD => X64_LARGE_PAGE_SIZE,
            Self::PT => X64_PAGE_SIZE,
        }
    }

    pub fn leaf(self, va: u64) -> PageTableEntryType {
        match self {
            Self::PML4 => panic!("cannot insert a leaf entry into a PML4 table"),
            Self::PDPT => PageTableEntryType::Leaf1GbPage(va),
            Self::PD => PageTableEntryType::Leaf2MbPage(va),
            Self::PT => PageTableEntryType::Leaf4kPage(va),
        }
    }

    fn pa_mask(self) -> u64 {
        match self {
            Self::PML4 => 0x000f_ffff_c000_0000,
            Self::PDPT => 0x000f_ffff_ffe0_0000,
            Self::PD => 0x000f_ffff_ffff_f000,
            Self::PT => 0x000f_ffff_ffff_f000,
        }
    }

    pub fn directory_pa(self, va: u64) -> u64 {
        va & self.pa_mask()
    }
}

pub trait PteOps {
    fn get_addr_mask(&self) -> u64;
    fn get_confidential_mask(&self) -> u64;

    fn build_pte(entry_type: PageTableEntryType, permissions: Option<u64>) -> PageTableEntry {
        let mut entry: u64 = X64_PTE_PRESENT | X64_PTE_ACCESSED;

        if let Some(permissions) = permissions {
            assert!(permissions == X64_PTE_READ_WRITE);
            entry |= permissions;
        }

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
    /// This sets the PTE to preset, accessed, dirty, execute.
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
#[derive(Clone, PartialEq, Eq, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PageTable {
    entries: [PageTableEntry; PAGE_TABLE_ENTRY_COUNT],
}

impl PageTable {
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut PageTableEntry> {
        self.entries.iter_mut()
    }

    /// Treat this page table as a page table of a given level, and locate the entry corresponding to a va.
    pub fn entry(&mut self, gva: u64, level: u8) -> &mut PageTableEntry {
        let index = get_amd64_pte_index(gva, level as u64) as usize;
        &mut self.entries[index]
    }
}

impl core::ops::Index<usize> for PageTable {
    type Output = PageTableEntry;

    fn index(&self, index: usize) -> &Self::Output {
        &self.entries[index]
    }
}

impl core::ops::IndexMut<usize> for PageTable {
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
struct PageTableBuilderParams {
    page_table_gpa: u64,
    confidential_bit: Option<u32>,
}

pub struct PageTableBuilder<'a> {
    params: PageTableBuilderParams,
    /// a reference to a mutable slice of PageTables, used as working memory for constructing the page table
    page_table: &'a mut [PageTable],
    /// a reference to a mutable slice of u8s, used to store and return the final page table bytes
    flattened_page_table: &'a mut [u8],
    /// a reference to a slice of ranges to map in the page table
    ranges: &'a [MappedRange],
}

impl PteOps for PageTableBuilderParams {
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

impl<'a> PageTableBuilder<'a> {
    /// Creates a new instance of PageTableBuilder, taking required arguments for working buffers and the page table gpa.
    pub fn new(
        page_table_gpa: u64,
        page_table: &'a mut [PageTable],
        flattened_page_table: &'a mut [u8],
        ranges: &'a [MappedRange],
    ) -> Result<Self, Error> {
        if flattened_page_table.len() != (page_table.len() * PAGE_TABLE_SIZE) {
            Err(Error::BadBufferSize {
                bytes_buf: flattened_page_table.len(),
                struct_buf: page_table.len() * PAGE_TABLE_SIZE,
            })
        } else {
            for (_, range) in ranges.iter().enumerate() {
                if range.start() > range.end() {
                    return Err(Error::UnsortedMappings);
                }
            }

            for (_, window) in ranges.windows(2).enumerate() {
                let (l, r) = (&window[0], &window[1]);

                if r.start() < l.start() {
                    return Err(Error::UnsortedMappings);
                }

                if l.end() > r.start() {
                    return Err(Error::OverlappingMappings);
                }
            }
            Ok(PageTableBuilder {
                params: PageTableBuilderParams {
                    page_table_gpa,
                    confidential_bit: None,
                },
                page_table,
                flattened_page_table,
                ranges,
            })
        }
    }

    pub fn with_confidential_bit(mut self, bit_position: u32) -> Self {
        self.params.confidential_bit = Some(bit_position);
        self
    }

    /// Build a set of X64 page tables identity mapping the given regions.
    /// This creates up to 3+N page tables: 1 PML4E and up to 2 PDPTE tables, and N page tables counted at 1 per GB of size,
    /// for 2MB mappings.
    pub fn build(self) -> Result<&'a [u8], Error> {
        let PageTableBuilder {
            page_table,
            flattened_page_table,
            ranges,
            params,
        } = self;

        // Allocate single PML4E page table.
        let (mut page_table_index, pml4_table_index) = (0, 0);
        let confidential = params.confidential_bit.is_some();

        // Allocate and link tables underneath the PML4E
        let mut link_tables = |start_va: u64, end_va: u64, permissions: Option<u64>| {
            let mut current_va = start_va;
            let mut get_or_insert_entry = |table_index: usize,
                                           entry_level: EntryLevel,
                                           current_va: &mut u64|
             -> Option<usize> {
                // If the current mapping can be inserted at this level as a leaf entry, do so
                if (*current_va).is_multiple_of(entry_level.mapping_size())
                    && (*current_va + entry_level.mapping_size() <= end_va)
                {
                    let entry = page_table[table_index].entry(*current_va, entry_level as u8);
                    assert!(!entry.is_present());

                    #[cfg(feature = "tracing")]
                    tracing::trace!(
                        "inserting entry for va: {:#X} at level {:?}",
                        current_va,
                        entry_level
                    );

                    let mut new_entry = PageTableBuilderParams::build_pte(
                        entry_level.leaf(*current_va),
                        permissions,
                    );
                    params.set_pte_confidentiality(&mut new_entry, confidential);
                    *entry = new_entry;
                    *current_va += entry_level.mapping_size() as u64;

                    None
                }
                // The current mapping cannot be inserted as a leaf at this level
                //
                // Find or create the appropriate directory at this hierarchy level, and
                // return the index
                else {
                    let directory_pa = entry_level.directory_pa(*current_va);
                    let entry = page_table[table_index].entry(directory_pa, entry_level as u8);

                    if !entry.is_present() {
                        page_table_index += 1;
                        // Allocate and link a page directory
                        let output_address =
                            params.page_table_gpa + page_table_index as u64 * X64_PAGE_SIZE;

                        let mut new_entry = PageTableBuilderParams::build_pte(
                            PageTableEntryType::Pde(output_address),
                            permissions,
                        );

                        #[cfg(feature = "tracing")]
                        tracing::trace!(
                            "creating directory for va: {:#X} at level {:?}",
                            directory_pa,
                            entry_level
                        );
                        params.set_pte_confidentiality(&mut new_entry, confidential);
                        *entry = new_entry;

                        Some(page_table_index)
                    } else {
                        Some(
                            ((params.get_addr_from_pte(entry) - params.page_table_gpa)
                                / X64_PAGE_SIZE)
                                .try_into()
                                .expect("Valid page table index"),
                        )
                    }
                }
            };

            while current_va < end_va {
                #[cfg(feature = "tracing")]
                tracing::trace!("creating entry for va: {:#X}", current_va);
                // For the current_va, insert entires as needed into the page table hierarchy,
                // terminating when a leaf entry is inserted
                get_or_insert_entry(pml4_table_index, EntryLevel::PML4, &mut current_va)
                    .and_then(|pdpte_table_index| {
                        get_or_insert_entry(pdpte_table_index, EntryLevel::PDPT, &mut current_va)
                    })
                    .and_then(|pde_table_index| {
                        get_or_insert_entry(pde_table_index, EntryLevel::PD, &mut current_va)
                    })
                    .and_then(|pt_table_index| {
                        get_or_insert_entry(pt_table_index, EntryLevel::PT, &mut current_va)
                    });
            }
        };

        for range in ranges {
            link_tables(range.start, range.end, range.permissions);
        }

        // flatten page table vec into u8 vec
        Ok(flatten_page_table(
            page_table,
            flattened_page_table,
            page_table_index + 1,
        ))
    }
}

#[derive(Debug, Clone)]
struct IdentityMapBuilderParams {
    page_table_gpa: u64,
    identity_map_size: IdentityMapSize,
    address_bias: u64,
    pml4e_link: Option<(u64, u64)>,
}

pub struct IdentityMapBuilder<'a> {
    params: IdentityMapBuilderParams,
    /// a reference to a mutable slice of PageTables, used as working memory for constructing the page table
    page_table: &'a mut [PageTable],
    /// a reference to a mutable slice of u8s, used to store and return the final page table bytes
    flattened_page_table: &'a mut [u8],
}

impl<'a> IdentityMapBuilder<'a> {
    pub fn new(
        page_table_gpa: u64,
        identity_map_size: IdentityMapSize,
        page_table: &'a mut [PageTable],
        flattened_page_table: &'a mut [u8],
    ) -> Result<Self, Error> {
        if flattened_page_table.len() != (page_table.len() * PAGE_TABLE_SIZE) {
            Err(Error::BadBufferSize {
                bytes_buf: flattened_page_table.len(),
                struct_buf: page_table.len() * PAGE_TABLE_SIZE,
            })
        } else {
            Ok(IdentityMapBuilder {
                params: IdentityMapBuilderParams {
                    page_table_gpa,
                    identity_map_size,
                    address_bias: 0,
                    pml4e_link: None,
                },
                page_table,
                flattened_page_table,
            })
        }
    }

    pub fn with_address_bias(mut self, address_bias: u64) -> Self {
        self.params.address_bias = address_bias;
        self
    }

    /// An optional PML4E entry may be linked, with arguments being (link_target_gpa, linkage_gpa).
    /// link_target_gpa represents the GPA of the PML4E to link into the built page table.
    /// linkage_gpa represents the GPA at which the linked PML4E should be linked.
    pub fn with_pml4e_link(mut self, pml4e_link: (u64, u64)) -> Self {
        self.params.pml4e_link = Some(pml4e_link);
        self
    }

    /// Build a set of X64 page tables identity mapping the bottom address
    /// space with an optional address bias.
    pub fn build(self) -> &'a [u8] {
        let IdentityMapBuilder {
            page_table,
            flattened_page_table,
            params,
        } = self;

        // Allocate page tables. There are up to 6 total page tables:
        //      1 PML4E (Level 4) (omitted if the address bias is non-zero)
        //      1 PDPTE (Level 3)
        //      4 or 8 PDE tables (Level 2)
        // Note that there are no level 1 page tables, as 2MB pages are used.
        let leaf_page_table_count = match params.identity_map_size {
            IdentityMapSize::Size4Gb => 4,
            IdentityMapSize::Size8Gb => 8,
        };
        let page_table_count = leaf_page_table_count + if params.address_bias == 0 { 2 } else { 1 };
        let mut page_table_allocator = page_table.iter_mut().enumerate();

        // Allocate single PDPTE table.
        let pdpte_table = if params.address_bias == 0 {
            // Allocate single PML4E page table.
            let (_, pml4e_table) = page_table_allocator
                .next()
                .expect("pagetable should always be available, code bug if not");

            // PDPTE table is the next pagetable.
            let (pdpte_table_index, pdpte_table) = page_table_allocator
                .next()
                .expect("pagetable should always be available, code bug if not");

            // Set PML4E entry linking PML4E to PDPTE.
            let output_address = params.page_table_gpa + pdpte_table_index as u64 * X64_PAGE_SIZE;
            pml4e_table.entries[0].set_entry(PageTableEntryType::Pde(output_address));

            // Set PML4E entry to link the additional entry if specified.
            if let Some((link_target_gpa, linkage_gpa)) = params.pml4e_link {
                assert!((linkage_gpa & 0x7FFFFFFFFF) == 0);
                pml4e_table.entries[linkage_gpa as usize >> 39]
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
        let top_address = match params.identity_map_size {
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
            let output_address = params.page_table_gpa + pde_table_index as u64 * X64_PAGE_SIZE;
            let pdpte_entry = &mut pdpte_table.entries[pdpte_index as usize];
            assert!(!pdpte_entry.is_present());
            pdpte_entry.set_entry(PageTableEntryType::Pde(output_address));

            // Set all 2MB entries in this PDE table.
            for entry in pde_table.iter_mut() {
                entry.set_entry(PageTableEntryType::Leaf2MbPage(
                    current_va + params.address_bias,
                ));
                current_va += X64_LARGE_PAGE_SIZE;
            }
        }

        // Flatten page table vec into u8 vec
        flatten_page_table(page_table, flattened_page_table, page_table_count)
    }
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

fn flatten_page_table<'a>(
    page_table: &mut [PageTable],
    flattened_page_table: &'a mut [u8],
    page_table_count: usize,
) -> &'a [u8] {
    for (page_table, dst) in page_table
        .iter()
        .take(page_table_count)
        .zip(flattened_page_table.chunks_mut(PAGE_TABLE_SIZE))
    {
        let src = page_table.as_bytes();
        dst.copy_from_slice(src);
    }

    &flattened_page_table[0..PAGE_TABLE_SIZE * page_table_count]
}

#[cfg(test)]
mod tests {
    use super::X64_1GB_PAGE_SIZE;
    use super::align_up_to_large_page_size;
    use super::align_up_to_page_size;
    use super::calculate_pde_table_count;

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
