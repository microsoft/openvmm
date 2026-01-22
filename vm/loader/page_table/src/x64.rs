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

/// An upper bound on the number of page tables that will be built for an x64 identity
/// map. The builder will greedily map the largest possible page size, a cap of 20 tables
/// is more than enough for mapping a few gigabytes with mostly large pages, which is
/// sufficient for all of the current use cases of the identity map builder
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
/// An x64 page table entry
pub struct PageTableEntry {
    pub(crate) entry: u64,
}

/// A memory range to be mapped in a page table, and the associated permissions
/// The default permissions bits are present, R/W, executable
#[derive(Copy, Clone, Debug)]
pub struct MappedRange {
    start: u64,
    end: u64,
    permissions: u64,
}

impl MappedRange {
    /// Create a new mapped range, with default permissions
    pub fn new(start: u64, end: u64) -> Self {
        Self {
            start,
            end,
            permissions: X64_PTE_PRESENT | X64_PTE_ACCESSED | X64_PTE_READ_WRITE,
        }
    }

    /// The start address of the mapped range
    pub fn start(&self) -> u64 {
        self.start
    }

    /// The end address of the mapped range
    pub fn end(&self) -> u64 {
        self.end
    }

    /// Consumes a mapped range, and returns the range without the writable bit set
    pub fn read_only(mut self) -> Self {
        self.permissions &= !X64_PTE_READ_WRITE;
        self
    }
}

impl core::fmt::Debug for PageTableEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PageTableEntry")
            .field("entry", &self.entry)
            .field("is_present", &self.is_present())
            .field("gpa", &self.gpa())
            .finish()
    }
}

#[derive(Debug, Copy, Clone)]
pub enum PageTableEntryType {
    /// 1GB page in a PDPT
    Leaf1GbPage(u64),
    /// 2MB page in a PD
    Leaf2MbPage(u64),
    /// 4K page in a PT
    Leaf4kPage(u64),
    /// A link to a lower level page table in a PML4, PDPT, or PD
    Pde(u64),
}

/// The depth of an x64 page table
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum EntryLevel {
    Pml4 = 3,
    Pdpt = 2,
    Pd = 1,
    Pt = 0,
}

impl EntryLevel {
    /// The amount of memory that can be mapped by an entry in a page table.
    /// If the entry is a leaf, this value represents the size of the entry,
    /// otherwise it represents the maximum amount of physical memory space
    /// that can be mapped if all mappings below this entry are used
    pub fn mapping_size(self) -> u64 {
        match self {
            Self::Pml4 => X64_1GB_PAGE_SIZE * 512,
            Self::Pdpt => X64_1GB_PAGE_SIZE,
            Self::Pd => X64_LARGE_PAGE_SIZE,
            Self::Pt => X64_PAGE_SIZE,
        }
    }

    /// Returns a leaf entry for a virtual address in this level
    pub fn leaf(self, va: u64) -> PageTableEntryType {
        match self {
            Self::Pml4 => panic!("cannot insert a leaf entry into a PML4 table"),
            Self::Pdpt => PageTableEntryType::Leaf1GbPage(va),
            Self::Pd => PageTableEntryType::Leaf2MbPage(va),
            Self::Pt => PageTableEntryType::Leaf4kPage(va),
        }
    }

    fn pa_mask(self) -> u64 {
        match self {
            Self::Pml4 => 0x000f_ffff_c000_0000,
            Self::Pdpt => 0x000f_ffff_ffe0_0000,
            Self::Pd => 0x000f_ffff_ffff_f000,
            Self::Pt => 0x000f_ffff_ffff_f000,
        }
    }

    /// Returns the physical address of the directory entry for a va at this level
    /// Assumes that the directory is part of an identity mapping
    pub fn directory_pa(self, va: u64) -> u64 {
        va & self.pa_mask()
    }
}

impl PageTableEntry {
    const VALID_BITS: u64 = 0x000f_ffff_ffff_f000;

    /// Set an AMD64 PDE to either represent a leaf 2MB page or PDE.
    /// This sets the PTE to present, accessed, dirty, read write execute.
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

    /// Checks if a page table entry is marked as present
    pub fn is_present(&self) -> bool {
        self.entry & X64_PTE_PRESENT == X64_PTE_PRESENT
    }

    /// Returns the GPA pointed to by a mapping, if it is present
    pub fn gpa(&self) -> Option<u64> {
        if self.is_present() {
            // bits 51 to 12 describe the gpa of the next page table
            Some(self.entry & Self::VALID_BITS)
        } else {
            None
        }
    }

    /// Clears the address in an entry, and replaces it with the provided address
    pub fn set_addr(&mut self, addr: u64) {
        assert!(addr & !Self::VALID_BITS == 0);

        // clear addr bits, set new addr
        self.entry &= !Self::VALID_BITS;
        self.entry |= addr;
    }

    /// Get the address pointed to by a page table entry, regardless of whether the entry is present
    pub fn get_addr(&self) -> u64 {
        self.entry & Self::VALID_BITS
    }

    /// Clear all bits of a page table entry
    pub fn clear(&mut self) {
        self.entry = 0;
    }
}

#[repr(C)]
#[derive(Clone, PartialEq, Eq, IntoBytes, Immutable, KnownLayout, FromBytes)]
/// A single page table at any level of the page table hierarchy
pub struct PageTable {
    entries: [PageTableEntry; PAGE_TABLE_ENTRY_COUNT],
}

impl PageTable {
    /// Returns a page table as a mutable iterator of page table entries
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
struct PageTableBuilderInner {
    page_table_gpa: u64,
    confidential_bit: Option<u32>,
}

/// A builder for an x64 identity-mapped page table
pub struct PageTableBuilder<'a> {
    /// parameters to the page table builder, stored seperately s.t. they can be easily cloned and copied
    inner: PageTableBuilderInner,
    /// a reference to a mutable slice of PageTables, used as working memory for constructing the page table
    page_table: &'a mut [PageTable],
    /// a reference to a mutable slice of u8s, used to store and return the final page table bytes
    flattened_page_table: &'a mut [u8],
    /// a reference to a slice of ranges to map in the page table
    ranges: &'a [MappedRange],
}

impl PageTableBuilderInner {
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

    fn build_pte(&self, entry_type: PageTableEntryType, permissions: u64) -> PageTableEntry {
        let mut entry: u64 = permissions;

        match entry_type {
            PageTableEntryType::Leaf1GbPage(address) => {
                // Must be 1GB aligned.
                assert_eq!(address % X64_1GB_PAGE_SIZE, 0);
                entry |= address;
                entry |= X64_PTE_LARGE_PAGE | X64_PTE_DIRTY;
            }
            PageTableEntryType::Leaf2MbPage(address) => {
                // Leaf entry, set like UEFI does for 2MB pages. Must be 2MB aligned.
                assert_eq!(address % X64_LARGE_PAGE_SIZE, 0);
                entry |= address;
                entry |= X64_PTE_LARGE_PAGE | X64_PTE_DIRTY;
            }
            PageTableEntryType::Leaf4kPage(address) => {
                // Must be 4K aligned.
                assert_eq!(address % X64_PAGE_SIZE, 0);
                entry |= address;
                entry |= X64_PTE_DIRTY;
            }
            PageTableEntryType::Pde(address) => {
                // Points to another pagetable.
                assert_eq!(address % X64_PAGE_SIZE, 0);
                entry |= address;
            }
        }

        let mask = self.get_confidential_mask();
        if self.confidential_bit.is_some() {
            entry |= mask;
        } else {
            entry &= !mask;
        }

        PageTableEntry { entry }
    }

    fn get_addr_from_pte(&self, pte: &PageTableEntry) -> u64 {
        pte.entry & self.get_addr_mask()
    }
}

impl<'a> PageTableBuilder<'a> {
    /// Creates a new instance of the page table builder. The [PageTable] slice is working memory
    /// for constructing the page table, and the [u8] slice is the memory used to output the
    /// final bytes of the page table
    ///
    /// The working memory and output memory are taken as parameters to allow for the caller
    /// to flexibly choose their allocation strategy, to support usage in no_std environments
    /// like openhcl_boot
    pub fn new(
        page_table_gpa: u64,
        page_table: &'a mut [PageTable],
        flattened_page_table: &'a mut [u8],
        ranges: &'a [MappedRange],
    ) -> Result<Self, Error> {
        // TODO: When const generic expressions are supported, the builder can take arrays
        // instead of slices, and this validation can be moved to the types
        if flattened_page_table.len() != (page_table.len() * PAGE_TABLE_SIZE) {
            Err(Error::BadBufferSize {
                bytes_buf: flattened_page_table.len(),
                struct_buf: page_table.len() * PAGE_TABLE_SIZE,
            })
        } else {
            for range in ranges.iter() {
                if range.start() > range.end() {
                    return Err(Error::InvalidRange);
                }
            }

            for window in ranges.windows(2) {
                let (l, r) = (&window[0], &window[1]);

                if r.start() < l.start() {
                    return Err(Error::UnsortedMappings);
                }

                if l.end() > r.start() {
                    return Err(Error::OverlappingMappings);
                }
            }
            Ok(PageTableBuilder {
                inner: PageTableBuilderInner {
                    page_table_gpa,
                    confidential_bit: None,
                },
                page_table,
                flattened_page_table,
                ranges,
            })
        }
    }

    /// Builds the page tables with the confidential bit set
    pub fn with_confidential_bit(mut self, bit_position: u32) -> Self {
        self.inner.confidential_bit = Some(bit_position);
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
            inner,
        } = self;

        // Allocate single PML4E page table.
        let (mut page_table_index, pml4_table_index) = (0, 0);

        // Allocate and link table
        let mut link_tables = |start_va: u64, end_va: u64, permissions: u64| -> Result<(), Error> {
            let mut current_va = start_va;
            let mut get_or_insert_entry = |table_index: usize,
                                           entry_level: EntryLevel,
                                           current_va: &mut u64|
             -> Result<Option<usize>, Error> {
                // If the current mapping can be inserted at this level as a leaf entry, do so
                if (*current_va).is_multiple_of(entry_level.mapping_size())
                    && (*current_va + entry_level.mapping_size() <= end_va)
                {
                    let entry = page_table[table_index].entry(*current_va, entry_level as u8);
                    if entry.is_present() {
                        // This error should be unreachable - something has gone horribly wrong
                        return Err(Error::AttemptedEntryOverwrite);
                    }

                    #[cfg(feature = "tracing")]
                    tracing::trace!(
                        "inserting entry for va: {:#X} at level {:?}",
                        current_va,
                        entry_level
                    );

                    let new_entry = inner.build_pte(entry_level.leaf(*current_va), permissions);
                    *entry = new_entry;
                    *current_va += entry_level.mapping_size();

                    Ok(None)
                }
                // The current mapping cannot be inserted as a leaf at this level
                //
                // Find or create the appropriate directory at this hierarchy level, and
                // return the index
                else {
                    let directory_pa = entry_level.directory_pa(*current_va);
                    let len = page_table.len();
                    let entry = page_table[table_index].entry(directory_pa, entry_level as u8);

                    if !entry.is_present() {
                        page_table_index += 1;

                        if page_table_index >= len {
                            return Err(Error::NotEnoughMemory);
                        }
                        // Allocate and link a page directory
                        let output_address =
                            inner.page_table_gpa + page_table_index as u64 * X64_PAGE_SIZE;

                        // Create the directory entry. Directory entries can be shared amongst
                        // MappedRanges with different sets of permissions, so give a wide set of
                        // permissions in the directory, and apply the MappedRange permissions in
                        // the leaf entry
                        let new_entry = inner.build_pte(
                            PageTableEntryType::Pde(output_address),
                            X64_PTE_PRESENT | X64_PTE_ACCESSED | X64_PTE_READ_WRITE,
                        );

                        #[cfg(feature = "tracing")]
                        tracing::trace!(
                            "creating directory for va: {:#X} at level {:?}",
                            directory_pa,
                            entry_level
                        );
                        *entry = new_entry;

                        Ok(Some(page_table_index))
                    } else {
                        Ok(Some(
                            ((inner.get_addr_from_pte(entry) - inner.page_table_gpa)
                                / X64_PAGE_SIZE)
                                .try_into()
                                .expect("Valid page table index"),
                        ))
                    }
                }
            };

            while current_va < end_va {
                #[cfg(feature = "tracing")]
                tracing::trace!("creating entry for va: {:#X}", current_va);
                // For the current_va, insert entires as needed into the page table hierarchy,
                // terminating when a leaf entry is inserted
                let pdpt_table_index =
                    get_or_insert_entry(pml4_table_index, EntryLevel::Pml4, &mut current_va)?;
                if let Some(pdpt_table_index) = pdpt_table_index {
                    let pd_table_index =
                        get_or_insert_entry(pdpt_table_index, EntryLevel::Pdpt, &mut current_va)?;
                    if let Some(pd_table_index) = pd_table_index {
                        let pt_table_index =
                            get_or_insert_entry(pd_table_index, EntryLevel::Pd, &mut current_va)?;
                        if let Some(pt_table_index) = pt_table_index {
                            get_or_insert_entry(pt_table_index, EntryLevel::Pt, &mut current_va)?;
                        }
                    }
                }
            }

            Ok(())
        };

        for range in ranges {
            link_tables(range.start, range.end, range.permissions)?;
        }

        // flatten the [page_table] into a [u8]
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

/// An IdentityMap Builder, which builds either a 4GB or 8GB identity map of the lower address space
/// FUTURE: This logic can merged with the PageTableBuilder, rather than maintaining two implementations
pub struct IdentityMapBuilder<'a> {
    params: IdentityMapBuilderParams,
    /// a reference to a mutable slice of PageTables, used as working memory for constructing the page table
    page_table: &'a mut [PageTable],
    /// a reference to a mutable slice of u8s, used to store and return the final page table bytes
    flattened_page_table: &'a mut [u8],
}

impl<'a> IdentityMapBuilder<'a> {
    /// Creates a new instance of the IdentityMapBuilder. The [PageTable] slice is working memory
    /// for constructing the page table, and the [u8] slice is the memory used to output the
    /// final bytes of the page table
    ///
    /// The working memory and output memory are taken as parameters to allow for the caller
    /// to flexibly choose their allocation strategy, to support usage in no_std environments
    /// like openhcl_boot
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

    /// Builds the page tables with an address bias, a fixed offset between the virtual
    /// and physical addresses in the identity map
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

        // Flatten [page_table] into [u8]
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
    use std;
    use std::vec;

    use super::Error;
    use super::MappedRange;
    use super::PAGE_TABLE_MAX_BYTES;
    use super::PAGE_TABLE_MAX_COUNT;
    use super::PageTable;
    use super::PageTableBuilder;
    use super::X64_1GB_PAGE_SIZE;
    use super::align_up_to_large_page_size;
    use super::align_up_to_page_size;
    use super::calculate_pde_table_count;
    use zerocopy::FromZeros;

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

    fn check_page_table_count(ranges: &[MappedRange], count: usize) {
        let mut page_table_work_buffer: Vec<PageTable> =
            vec![PageTable::new_zeroed(); PAGE_TABLE_MAX_COUNT];
        let mut page_table: Vec<u8> = vec![0; PAGE_TABLE_MAX_BYTES];

        let page_table_builder = PageTableBuilder::new(
            0,
            page_table_work_buffer.as_mut_slice(),
            page_table.as_mut_slice(),
            ranges,
        )
        .expect("page table builder initialization should succeed");

        let page_table = page_table_builder.build().expect("building should succeed");
        assert_eq!(page_table.len(), count);
    }

    fn page_table_builder_error(ranges: &[MappedRange]) -> Option<Error> {
        let mut page_table_work_buffer: Vec<PageTable> =
            vec![PageTable::new_zeroed(); PAGE_TABLE_MAX_COUNT];
        let mut page_table: Vec<u8> = vec![0; PAGE_TABLE_MAX_BYTES];

        PageTableBuilder::new(
            0,
            page_table_work_buffer.as_mut_slice(),
            page_table.as_mut_slice(),
            ranges,
        )
        .err()
    }

    #[test]
    fn test_page_table_entry_sizing() {
        const ONE_GIG: u64 = 1024 * 1024 * 1024;
        const TWO_MB: u64 = 1024 * 1024 * 2;
        const FOUR_KB: u64 = 4096;

        check_page_table_count(&[MappedRange::new(0, ONE_GIG)], 4096 * 2);
        check_page_table_count(&[MappedRange::new(0, TWO_MB)], 4096 * 3);
        check_page_table_count(&[MappedRange::new(0, FOUR_KB)], 4096 * 4);
        check_page_table_count(&[MappedRange::new(FOUR_KB, ONE_GIG)], 4096 * 4);
        check_page_table_count(&[MappedRange::new(TWO_MB, ONE_GIG)], 4096 * 3);
        check_page_table_count(&[MappedRange::new(TWO_MB, ONE_GIG + FOUR_KB)], 4096 * 5);
        check_page_table_count(&[MappedRange::new(TWO_MB, ONE_GIG + TWO_MB)], 4096 * 4);
    }

    #[test]
    fn test_page_table_builder_overlapping_range() {
        const ONE_GIG: u64 = 1024 * 1024 * 1024;
        const TWO_MB: u64 = 1024 * 1024 * 2;
        const FOUR_KB: u64 = 4096;

        let err = page_table_builder_error(&[
            MappedRange::new(FOUR_KB, ONE_GIG),
            MappedRange::new(TWO_MB, ONE_GIG),
        ])
        .expect("must fail");
        assert!(matches!(err, Error::OverlappingMappings));
    }

    #[test]
    fn test_page_table_builder_invalid_range() {
        const ONE_GIG: u64 = 1024 * 1024 * 1024;
        const FOUR_KB: u64 = 4096;

        let err =
            page_table_builder_error(&[MappedRange::new(ONE_GIG, FOUR_KB)]).expect("must fail");
        assert!(matches!(err, Error::InvalidRange));
    }

    #[test]
    fn test_page_table_builder_oom() {
        const ONE_GIG: u64 = 1024 * 1024 * 1024;

        let mut page_table_work_buffer: Vec<PageTable> = vec![PageTable::new_zeroed(); 1];
        let mut page_table: Vec<u8> = vec![0; 4096];

        let err = PageTableBuilder::new(
            0,
            page_table_work_buffer.as_mut_slice(),
            page_table.as_mut_slice(),
            &[MappedRange::new(0, ONE_GIG)],
        )
        .expect("page table builder initialization should succeed")
        .build()
        .expect_err("building page tables should fail");

        assert!(matches!(err, Error::NotEnoughMemory));
    }

    #[test]
    fn test_page_table_builder_mismatched_buffers() {
        const ONE_GIG: u64 = 1024 * 1024 * 1024;

        let mut page_table_work_buffer: Vec<PageTable> = vec![PageTable::new_zeroed(); 4];
        let mut page_table: Vec<u8> = vec![0; 4096 * 5];

        let err = PageTableBuilder::new(
            0,
            page_table_work_buffer.as_mut_slice(),
            page_table.as_mut_slice(),
            &[MappedRange::new(0, ONE_GIG)],
        )
        .err()
        .expect("building page tables should fail");

        assert!(matches!(
            err,
            Error::BadBufferSize {
                bytes_buf: _,
                struct_buf: _
            }
        ));
    }
}
