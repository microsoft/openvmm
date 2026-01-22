// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Methods to construct page tables on Aarch64.

use bitfield_struct::bitfield;

/// Some memory attributes. Refer to the ARM VMSA
/// manual for further details and other types.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Default)]
#[repr(u8)]
#[expect(non_camel_case_types)]
pub enum MemoryAttributeEl1 {
    /// Most restricted device memory: non-gathering,
    /// non-reordering, non-early-ack.
    #[default]
    Device_nGnRnE = 0,
    /// Program memory that can be read from and written to,
    /// accesses can be batched, reordered and early ack'ed,
    /// non-cacheable.
    Normal_NonCacheable = 0x44,
    /// Program memory that can be read from and written to,
    /// accesses can be batched, reordered and early ack'ed,
    /// write-through.
    Normal_WriteThrough = 0xbb,
    /// Program memory that can be read from and written to,
    /// accesses can be batched, reordered and early ack'ed.
    Normal_WriteBack = 0xff,
}

impl From<u8> for MemoryAttributeEl1 {
    fn from(value: u8) -> Self {
        match value {
            0 => MemoryAttributeEl1::Device_nGnRnE,
            0x44 => MemoryAttributeEl1::Normal_NonCacheable,
            0xbb => MemoryAttributeEl1::Normal_WriteThrough,
            0xff => MemoryAttributeEl1::Normal_WriteBack,
            _ => panic!("memory type is not supported"),
        }
    }
}

/// Legal indexes for memory attributes for aarch64 PTEs.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[repr(u64)]
pub enum MemoryAttributeIndex {
    Index0,
    Index1,
    Index2,
    Index3,
    Index4,
    Index5,
    Index6,
    Index7,
}

impl MemoryAttributeIndex {
    const fn from_bits(value: u64) -> Self {
        match value {
            0 => MemoryAttributeIndex::Index0,
            1 => MemoryAttributeIndex::Index1,
            2 => MemoryAttributeIndex::Index2,
            3 => MemoryAttributeIndex::Index3,
            4 => MemoryAttributeIndex::Index4,
            5 => MemoryAttributeIndex::Index5,
            6 => MemoryAttributeIndex::Index6,
            7 => MemoryAttributeIndex::Index7,
            _ => panic!("illegal state when looking for memory attribute index"),
        }
    }

    const fn into_bits(value: Self) -> u64 {
        value as u64
    }
}

impl From<MemoryAttributeIndex> for u64 {
    fn from(value: MemoryAttributeIndex) -> Self {
        MemoryAttributeIndex::into_bits(value)
    }
}

impl From<u64> for MemoryAttributeIndex {
    fn from(value: u64) -> Self {
        Self::from_bits(value)
    }
}

impl From<usize> for MemoryAttributeIndex {
    fn from(value: usize) -> Self {
        Self::from_bits(value as u64)
    }
}

/// aarch64 MAIR_EL1 register, provides indices
/// to use in the PTEs for memory types
#[derive(Debug, PartialEq, Eq, Clone, Copy, Default)]
pub struct MemoryAttributeIndirectionEl1(pub [MemoryAttributeEl1; 8]);

impl MemoryAttributeIndirectionEl1 {
    pub fn index_of(&self, needle: MemoryAttributeEl1) -> Option<MemoryAttributeIndex> {
        for (idx, &attr) in self.0.iter().enumerate() {
            if attr == needle {
                return Some(idx.into());
            }
        }

        None
    }
}

impl From<MemoryAttributeIndirectionEl1> for u64 {
    fn from(value: MemoryAttributeIndirectionEl1) -> Self {
        u64::from_le_bytes(value.0.map(|x| x as u8))
    }
}

impl From<u64> for MemoryAttributeIndirectionEl1 {
    fn from(value: u64) -> Self {
        MemoryAttributeIndirectionEl1(value.to_le_bytes().map(|x| x.into()))
    }
}

#[bitfield(u64)]
pub struct Aarch64PageTableEntry {
    pub valid: bool,
    pub table: bool, // Use PageBlockEntry if `false`
    #[bits(10)]
    _mbz0: u64,
    #[bits(35)]
    pub next_table_pfn: u64,
    #[bits(12)]
    _mbz1: u64,
    pub priv_x_never: bool,
    pub user_x_never: bool,
    // NoEffect = 0b00,
    // PrivOnly = 0b01,
    // ReadOnly = 0b10,
    // PrivReadOnly = 0b11
    #[bits(2)]
    pub access_perm: u64,
    pub non_secure: bool,
}

#[bitfield(u64)]
pub struct Aarch64PageBlockEntry {
    pub valid: bool,
    pub page: bool,
    #[bits(3)]
    pub mair_idx: MemoryAttributeIndex,
    #[bits(1)]
    _mbz0: u64,
    // PrivOnly = 0b00,
    // ReadWrite = 0b01,
    // PrivReadOnly = 0b10,
    // ReadOnly = 0b11
    #[bits(2)]
    pub access_perm: u64,
    // NonShareable = 0b00,
    // OuterShareable = 0b10,
    // InnerShareable = 0b11
    #[bits(2)]
    pub share_perm: u64,
    pub accessed: bool,
    pub not_global: bool,
    #[bits(35)]
    pub address_pfn: u64,
    #[bits(4)]
    _mbz1: u64,
    pub dirty: bool,
    pub contig: bool,
    pub priv_x_never: bool,
    pub user_x_never: bool,
    #[bits(9)]
    _mbz2: u64,
}

#[bitfield(u64)]
pub struct Arm64PageTableEntry {
    pub valid: bool,
    pub table: bool, // Use PageBlockEntry if `false`
    #[bits(10)]
    _mbz0: u64,
    #[bits(35)]
    pub next_table_pfn: u64,
    #[bits(12)]
    _mbz1: u64,
    pub priv_x_never: bool,
    pub user_x_never: bool,
    // NoEffect = 0b00,
    // PrivOnly = 0b01,
    // ReadOnly = 0b10,
    // PrivReadOnly = 0b11
    #[bits(2)]
    pub access_perm: u64,
    pub non_secure: bool,
}

#[bitfield(u64)]
pub struct Arm64PageBlockEntry {
    pub valid: bool,
    pub page: bool,
    #[bits(3)]
    pub mair_idx: usize,
    #[bits(1)]
    _mbz0: u64,
    // PrivOnly = 0b00,
    // ReadWrite = 0b01,
    // PrivReadOnly = 0b10,
    // ReadOnly = 0b11
    #[bits(2)]
    pub access_perm: u64,
    // NonShareable = 0b00,
    // OuterShareable = 0b10,
    // InnerShareable = 0b11
    #[bits(2)]
    pub share_perm: u64,
    pub accessed: bool,
    pub not_global: bool,
    #[bits(35)]
    pub address_pfn: u64,
    #[bits(4)]
    _mbz1: u64,
    pub dirty: bool,
    pub contig: bool,
    pub priv_x_never: bool,
    pub user_x_never: bool,
    #[bits(9)]
    _mbz2: u64,
}

#[bitfield(u64)]
pub struct VirtualAddress {
    #[bits(12)]
    pub offset: u64,
    #[bits(9)]
    pub lvl3: usize,
    #[bits(9)]
    pub lvl2: usize,
    #[bits(9)]
    pub lvl1: usize,
    #[bits(9)]
    pub lvl0: usize,
    #[bits(16)]
    pub asid: usize,
}

impl VirtualAddress {
    pub fn is_canonical(&self) -> bool {
        // The 16 most significant bits must be eqial to the 47th one.
        ((self.0 as i64) << 16 >> 16) == self.0 as i64
    }

    pub fn lvl_index(&self, index: usize) -> usize {
        match index {
            3 => self.lvl3(),
            2 => self.lvl2(),
            1 => self.lvl1(),
            0 => self.lvl0(),
            _ => panic!("invalid VA level index"),
        }
    }
}

const PAGE_SHIFT_4K: u64 = 12;
const PAGE_SHIFT_2M: u64 = 21;
const PAGE_SHIFT_1G: u64 = 30;

const PAGE_SIZE_4K: u64 = 1 << PAGE_SHIFT_4K;
const PAGE_SIZE_2M: u64 = 1 << PAGE_SHIFT_2M;
const PAGE_SIZE_1G: u64 = 1 << PAGE_SHIFT_1G;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arm64PageMapError {
    OutOfMemory,
    NonCanonicalVirtAddress,
    MisalignedVirtAddress,
    MisalignedPhysAddress,
    InvalidMappingSize,
    EmptyMapping,
    AlreadyMapped,
}

#[derive(Debug, Clone, Copy)]
#[repr(u64)]
pub enum Arm64PageSize {
    Small = PAGE_SIZE_4K,
    Large = PAGE_SIZE_2M,
    Huge = PAGE_SIZE_1G,
}

impl From<Arm64PageSize> for u64 {
    fn from(value: Arm64PageSize) -> Self {
        value as u64
    }
}

impl From<Arm64PageSize> for usize {
    fn from(value: Arm64PageSize) -> Self {
        value as usize
    }
}

const fn align_up(x: u64, page_size: Arm64PageSize) -> u64 {
    let ones_enough = page_size as u64 - 1;
    (x + ones_enough) & !ones_enough
}

const fn align_down(x: u64, page_size: Arm64PageSize) -> u64 {
    let ones_enough = page_size as u64 - 1;
    x & !ones_enough
}

const fn aligned(x: u64, page_size: Arm64PageSize) -> bool {
    let ones_enough = page_size as u64 - 1;
    (x & ones_enough) == 0
}

#[derive(Debug, Copy, Clone)]
pub enum Arm64NoExecute {
    Off,
    UserOnly,
    PrivilegedOnly,
    Full,
}

impl Arm64PageBlockEntry {
    pub fn set_xn(&mut self, xn: Arm64NoExecute) {
        match xn {
            Arm64NoExecute::Off => {
                self.set_user_x_never(false);
                self.set_priv_x_never(false);
            }
            Arm64NoExecute::UserOnly => {
                self.set_user_x_never(true);
                self.set_priv_x_never(false);
            }
            Arm64NoExecute::PrivilegedOnly => {
                self.set_user_x_never(false);
                self.set_priv_x_never(true);
            }
            Arm64NoExecute::Full => {
                self.set_user_x_never(true);
                self.set_priv_x_never(true);
            }
        }
    }
}

#[derive(Debug)]
pub struct Arm64PageTableSpace<'a> {
    /// Physical address at which the page table area starts.
    /// The root page tables will be placed at this address.
    phys_page_table_root: usize,
    /// The memory occupied by page tables.
    space: &'a mut [u8],
    /// Physical address of the next free 4KiB-aligned block in the
    /// `space`. This is essentially a bump allocator for the memory
    /// used by the page tables.
    brk: usize,
    /// Statistics of page tables allocations for each level.
    /// `lvl_stats[0]` is going to be always `1`.
    lvl_stats: [usize; 4],
}

impl<'a> Arm64PageTableSpace<'a> {
    pub fn new(phys_start: usize, space: &'a mut [u8]) -> Result<Self, Arm64PageMapError> {
        if !aligned(phys_start as u64, Arm64PageSize::Small) {
            return Err(Arm64PageMapError::MisalignedPhysAddress);
        }
        if !aligned(space.len() as u64, Arm64PageSize::Small) {
            return Err(Arm64PageMapError::InvalidMappingSize);
        }
        if space.is_empty() {
            return Err(Arm64PageMapError::EmptyMapping);
        }

        // Situate the root table at the beginning,
        // and initialize it with a value that makes pages appear as
        // non-present (at least on x64 and aarch64).
        space[..PAGE_SIZE_4K as usize].fill(0xfe);

        Ok(Self {
            phys_page_table_root: phys_start,
            space,
            brk: phys_start + PAGE_SIZE_4K as usize,
            lvl_stats: [1, 0, 0, 0],
        })
    }

    fn allocate_page_table(&mut self, level: usize) -> Result<u64, Arm64PageMapError> {
        if self.brk >= self.phys_page_table_root + self.space.len() {
            return Err(Arm64PageMapError::OutOfMemory);
        }
        let page_table_phys_addr = self.brk;
        self.brk += PAGE_SIZE_4K as usize;
        self.lvl_stats[level] += 1;

        Ok(page_table_phys_addr as u64)
    }

    pub fn used_space(&self) -> usize {
        self.brk - self.phys_page_table_root
    }

    pub fn lvl_stats(&self) -> [usize; 4] {
        self.lvl_stats
    }

    fn read_entry(&self, phys_table_start: u64, index: usize) -> u64 {
        debug_assert!(
            (phys_table_start as usize) < self.phys_page_table_root + self.space.len()
                && (phys_table_start as usize) >= self.phys_page_table_root
        );
        debug_assert!(aligned(phys_table_start, Arm64PageSize::Small));
        debug_assert!(index < PAGE_SIZE_4K as usize / size_of::<Arm64PageTableEntry>());

        let pos = phys_table_start as usize - self.phys_page_table_root
            + index * size_of::<Arm64PageTableEntry>();
        u64::from_le_bytes([
            self.space[pos],
            self.space[pos + 1],
            self.space[pos + 2],
            self.space[pos + 3],
            self.space[pos + 4],
            self.space[pos + 5],
            self.space[pos + 6],
            self.space[pos + 7],
        ])
    }

    fn write_entry(&mut self, phys_table_start: u64, index: usize, entry: u64) {
        debug_assert!(
            (phys_table_start as usize) < self.phys_page_table_root + self.space.len()
                && (phys_table_start as usize) >= self.phys_page_table_root
        );
        debug_assert!(aligned(phys_table_start, Arm64PageSize::Small));
        debug_assert!(index < PAGE_SIZE_4K as usize / size_of::<Arm64PageTableEntry>());

        #[cfg(feature = "tracing")]
        tracing::debug!(
            "Writing page table entry {entry:#016x}, index {index:#x}, table {phys_table_start:#x}"
        );

        let pos = phys_table_start as usize - self.phys_page_table_root
            + index * size_of::<Arm64PageTableEntry>();
        self.space[pos..pos + 8].copy_from_slice(&entry.to_le_bytes());
    }

    fn check_addresses_and_map_size(
        &self,
        phys_addr: u64,
        virt_addr: VirtualAddress,
        page_size: Arm64PageSize,
    ) -> Result<(), Arm64PageMapError> {
        if virt_addr.offset() != 0 {
            return Err(Arm64PageMapError::MisalignedVirtAddress);
        }
        if !virt_addr.is_canonical() {
            return Err(Arm64PageMapError::NonCanonicalVirtAddress);
        }

        if !aligned(phys_addr, page_size) {
            return Err(Arm64PageMapError::MisalignedPhysAddress);
        }
        if !aligned(virt_addr.0, page_size) {
            return Err(Arm64PageMapError::MisalignedVirtAddress);
        }

        Ok(())
    }

    fn map_page(
        &mut self,
        phys_addr: u64,
        virt_addr: VirtualAddress,
        memory_attribute_index: MemoryAttributeIndex,
        page_size: Arm64PageSize,
        xn: Arm64NoExecute,
    ) -> Result<(), Arm64PageMapError> {
        let mut table_phys_addr = self.phys_page_table_root as u64;
        let mut level = 0;
        let leaf_level = match page_size {
            Arm64PageSize::Small => 3,
            Arm64PageSize::Large => 2,
            Arm64PageSize::Huge => 1,
        };
        while level < leaf_level {
            let mut table_entry = Arm64PageTableEntry::from(
                self.read_entry(table_phys_addr, virt_addr.lvl_index(level)),
            );

            if table_entry.valid() && !table_entry.table() {
                return Err(Arm64PageMapError::AlreadyMapped);
            }

            if !table_entry.valid() {
                let next_table_phys_addr = self.allocate_page_table(level + 1)?;

                table_entry = Arm64PageTableEntry::new()
                    .with_valid(true)
                    .with_table(true)
                    .with_next_table_pfn(next_table_phys_addr >> PAGE_SHIFT_4K);

                self.write_entry(
                    table_phys_addr,
                    virt_addr.lvl_index(level),
                    table_entry.into(),
                );
            }
            table_phys_addr = table_entry.next_table_pfn() << PAGE_SHIFT_4K;

            level += 1;
        }

        let mut page_entry =
            Arm64PageBlockEntry::from(self.read_entry(table_phys_addr, virt_addr.lvl_index(level)));
        if page_entry.valid() {
            return Err(Arm64PageMapError::AlreadyMapped);
        }

        // Without setting the `accessed` flag, qemu fails translation
        // if the HA flag is not enabled in the TCR register. Support for
        // HA in indicated in the MMU features register #1.

        page_entry = Arm64PageBlockEntry::new()
            .with_valid(true)
            .with_page(leaf_level == 3)
            .with_accessed(true)
            .with_share_perm(3)
            .with_mair_idx(memory_attribute_index as usize)
            .with_address_pfn(phys_addr >> PAGE_SHIFT_4K);
        page_entry.set_xn(xn);

        self.write_entry(
            table_phys_addr,
            virt_addr.lvl_index(level),
            page_entry.into(),
        );

        Ok(())
    }

    pub fn map_pages(
        &mut self,
        phys_addr: u64,
        virt_addr: VirtualAddress,
        page_count: usize,
        page_size: Arm64PageSize,
        memory_attribute_index: MemoryAttributeIndex,
        xn: Arm64NoExecute,
    ) -> Result<(), Arm64PageMapError> {
        self.check_addresses_and_map_size(phys_addr, virt_addr, page_size)?;

        if page_count == 0 {
            return Err(Arm64PageMapError::EmptyMapping);
        }

        let pages_to_map = page_count;
        let mut pages_mapped = 0;
        let mut phys_addr = phys_addr;
        let mut virt_addr = virt_addr.0;
        while pages_mapped < pages_to_map {
            self.map_page(
                phys_addr,
                VirtualAddress(virt_addr),
                memory_attribute_index,
                page_size,
                xn,
            )?;

            pages_mapped += 1;
            phys_addr += page_size as u64;
            virt_addr += page_size as u64;
        }

        Ok(())
    }

    fn get_page_size_and_page_count(
        &self,
        non_mapped: u64,
        phys_addr: u64,
        virt_addr: u64,
    ) -> (Arm64PageSize, u64) {
        // Try larger pages first, then the next large page.
        // The goal is to spend as few page tables as possible.

        if aligned(phys_addr, Arm64PageSize::Huge)
            && aligned(virt_addr, Arm64PageSize::Huge)
            && non_mapped >= PAGE_SIZE_1G
        {
            (Arm64PageSize::Huge, non_mapped / Arm64PageSize::Huge as u64)
        } else if aligned(phys_addr, Arm64PageSize::Large)
            && aligned(virt_addr, Arm64PageSize::Large)
            && non_mapped >= PAGE_SIZE_2M
        {
            let before_huge_page = align_up(virt_addr, Arm64PageSize::Huge) - virt_addr;
            let page_count = align_down(
                if before_huge_page > 0 && before_huge_page < non_mapped {
                    before_huge_page
                } else {
                    non_mapped
                },
                Arm64PageSize::Large,
            ) / Arm64PageSize::Large as u64;

            (Arm64PageSize::Large, page_count)
        } else {
            let before_huge_page = align_up(virt_addr, Arm64PageSize::Huge) - virt_addr;
            let page_count = if before_huge_page > 0 && before_huge_page < non_mapped {
                before_huge_page
            } else {
                let before_large_page = align_up(virt_addr, Arm64PageSize::Large) - virt_addr;
                if before_large_page > 0 && before_large_page < non_mapped {
                    before_large_page
                } else {
                    non_mapped
                }
            } / Arm64PageSize::Small as u64;

            (Arm64PageSize::Small, page_count)
        }
    }

    pub fn map_range(
        &mut self,
        phys_addr: u64,
        virt_addr: VirtualAddress,
        size: u64,
        memory_attribute_index: MemoryAttributeIndex,
        xn: Arm64NoExecute,
    ) -> Result<(), Arm64PageMapError> {
        if !aligned(phys_addr, Arm64PageSize::Small) {
            return Err(Arm64PageMapError::MisalignedPhysAddress);
        }
        if !aligned(size, Arm64PageSize::Small) {
            return Err(Arm64PageMapError::InvalidMappingSize);
        }
        if size == 0 {
            return Err(Arm64PageMapError::EmptyMapping);
        }
        if virt_addr.offset() != 0 {
            return Err(Arm64PageMapError::MisalignedVirtAddress);
        }
        if !virt_addr.is_canonical() {
            return Err(Arm64PageMapError::NonCanonicalVirtAddress);
        }

        let mut non_mapped = size;
        let mut phys_addr = phys_addr;
        let mut virt_addr = virt_addr.into();

        let mut mapped = 0;
        while mapped < size {
            let (page_size, page_count) =
                self.get_page_size_and_page_count(non_mapped, phys_addr, virt_addr);
            self.map_pages(
                phys_addr,
                VirtualAddress(virt_addr),
                page_count as usize,
                page_size,
                memory_attribute_index,
                xn,
            )?;

            let just_mapped = page_count * page_size as u64;
            mapped += just_mapped;
            non_mapped -= just_mapped;
            phys_addr += just_mapped;
            virt_addr += just_mapped;
        }

        debug_assert!(mapped == size);
        debug_assert!(non_mapped == 0);
        Ok(())
    }
}

/// Build a set of Aarch64 page tables identity mapping the given region.
pub fn build_identity_page_tables_aarch64(
    page_table_gpa: u64,
    start_gpa: u64,
    size: u64,
    memory_attribute_indirection: MemoryAttributeIndirectionEl1,
    page_table_space: &mut [u8],
) -> &[u8] {
    // start_gpa and size must be 2MB aligned.
    if !aligned(start_gpa, Arm64PageSize::Large) {
        panic!("start_gpa not 2mb aligned");
    }

    if !aligned(size, Arm64PageSize::Large) {
        panic!("size not 2mb aligned");
    }

    #[cfg(feature = "tracing")]
    tracing::debug!(
        "Creating Aarch64 page tables at {page_table_gpa:#x} mapping starting at {start_gpa:#x} of size {size} bytes"
    );

    let mut page_tables =
        Arm64PageTableSpace::new(page_table_gpa as usize, page_table_space).unwrap();
    page_tables
        .map_range(
            start_gpa,
            VirtualAddress(start_gpa),
            size,
            memory_attribute_indirection
                .index_of(MemoryAttributeEl1::Normal_WriteBack)
                .unwrap(),
            Arm64NoExecute::UserOnly,
        )
        .unwrap();

    let used_space = page_tables.used_space();

    #[cfg(feature = "tracing")]
    {
        tracing::debug!("Page tables use {used_space} bytes");
        tracing::debug!("Page tables stats by level: {:?}", page_tables.lvl_stats());
    }

    &page_table_space[0..used_space]
}

#[cfg(test)]
mod tests {
    use std;

    use super::*;
    use std::vec;

    const DUMP_PAGE_TABLES: bool = false;

    #[test]
    fn test_mmu_small_pages() {
        let mut space = vec![0xaa; 0x100000];
        let mut page_tables = Arm64PageTableSpace::new(0x00000040248000, &mut space)
            .expect("Can initialize page tables");

        let mair_el1 = MemoryAttributeIndirectionEl1([
            MemoryAttributeEl1::Device_nGnRnE,
            MemoryAttributeEl1::Normal_NonCacheable,
            MemoryAttributeEl1::Normal_WriteThrough,
            MemoryAttributeEl1::Normal_WriteBack,
            MemoryAttributeEl1::Device_nGnRnE,
            MemoryAttributeEl1::Device_nGnRnE,
            MemoryAttributeEl1::Device_nGnRnE,
            MemoryAttributeEl1::Device_nGnRnE,
        ]);

        let wb_index = mair_el1
            .index_of(MemoryAttributeEl1::Normal_WriteBack)
            .expect("must be some WB memory available");

        let res = page_tables.map_pages(
            0x4000,
            VirtualAddress::from(0x4000),
            1,
            Arm64PageSize::Small,
            wb_index,
            Arm64NoExecute::Full,
        );
        assert_eq!(res, Ok(()));
        assert_eq!(page_tables.lvl_stats(), [1, 1, 1, 1]);

        let res = page_tables.map_pages(
            0x5000,
            VirtualAddress::from(0x5000),
            1,
            Arm64PageSize::Small,
            wb_index,
            Arm64NoExecute::Full,
        );
        assert_eq!(res, Ok(()));
        assert_eq!(page_tables.lvl_stats(), [1, 1, 1, 1]);

        let res = page_tables.map_pages(
            0x200000,
            VirtualAddress::from(0x200000),
            1,
            Arm64PageSize::Small,
            wb_index,
            Arm64NoExecute::Full,
        );
        assert_eq!(res, Ok(()));
        assert_eq!(page_tables.lvl_stats(), [1, 1, 1, 2]);

        let res = page_tables.map_pages(
            0x201000,
            VirtualAddress::from(0x201000),
            1,
            Arm64PageSize::Small,
            wb_index,
            Arm64NoExecute::Full,
        );
        assert_eq!(res, Ok(()));
        assert_eq!(page_tables.lvl_stats(), [1, 1, 1, 2]);

        let res = page_tables.map_pages(
            0x4000,
            VirtualAddress::from(0xffff_8000_0000_4000),
            1,
            Arm64PageSize::Small,
            wb_index,
            Arm64NoExecute::Full,
        );
        assert_eq!(res, Ok(()));
        assert_eq!(page_tables.lvl_stats(), [1, 2, 2, 3]);

        let res = page_tables.map_pages(
            0x5000,
            VirtualAddress::from(0xffff_8000_0000_5000),
            1,
            Arm64PageSize::Small,
            wb_index,
            Arm64NoExecute::Full,
        );
        assert_eq!(res, Ok(()));
        assert_eq!(page_tables.lvl_stats(), [1, 2, 2, 3]);

        let res = page_tables.map_pages(
            0x4000_0000,
            VirtualAddress::from(0x4000_0000),
            0x200,
            Arm64PageSize::Small,
            wb_index,
            Arm64NoExecute::Full,
        );
        assert_eq!(res, Ok(()));
        assert_eq!(page_tables.lvl_stats(), [1, 2, 3, 4]);

        if DUMP_PAGE_TABLES {
            std::fs::write("page_tables.bin", space).expect("can dump the page tables");
        }
    }

    #[test]
    fn test_mmu_large_pages() {
        let mut space = vec![0xaa; 0x100000];
        let mut page_tables = Arm64PageTableSpace::new(0x00000040248000, &mut space)
            .expect("Can initialize page tables");

        let mair_el1 = MemoryAttributeIndirectionEl1([
            MemoryAttributeEl1::Device_nGnRnE,
            MemoryAttributeEl1::Normal_NonCacheable,
            MemoryAttributeEl1::Normal_WriteThrough,
            MemoryAttributeEl1::Normal_WriteBack,
            MemoryAttributeEl1::Device_nGnRnE,
            MemoryAttributeEl1::Device_nGnRnE,
            MemoryAttributeEl1::Device_nGnRnE,
            MemoryAttributeEl1::Device_nGnRnE,
        ]);

        let wb_index = mair_el1
            .index_of(MemoryAttributeEl1::Normal_WriteBack)
            .expect("must be some WB memory available");

        let res = page_tables.map_pages(
            0,
            VirtualAddress::from(0),
            0x2000,
            Arm64PageSize::Large,
            wb_index,
            Arm64NoExecute::Full,
        );
        assert_eq!(res, Ok(()));
        assert_eq!(page_tables.lvl_stats(), [1, 1, 16, 0]);

        let res = page_tables.map_pages(
            0x4000,
            VirtualAddress::from(0x4000),
            4,
            Arm64PageSize::Small,
            wb_index,
            Arm64NoExecute::Full,
        );
        assert_eq!(res, Err(Arm64PageMapError::AlreadyMapped));
        assert_eq!(page_tables.lvl_stats(), [1, 1, 16, 0]);

        if DUMP_PAGE_TABLES {
            std::fs::write("page_tables_large.bin", space).expect("can dump the page tables");
        }
    }

    #[test]
    fn test_mmu_huge_pages() {
        let mut space = vec![0xaa; 0x100000];
        let mut page_tables = Arm64PageTableSpace::new(0x00000040248000, &mut space)
            .expect("Can initialize page tables");

        let mair_el1 = MemoryAttributeIndirectionEl1([
            MemoryAttributeEl1::Device_nGnRnE,
            MemoryAttributeEl1::Normal_NonCacheable,
            MemoryAttributeEl1::Normal_WriteThrough,
            MemoryAttributeEl1::Normal_WriteBack,
            MemoryAttributeEl1::Device_nGnRnE,
            MemoryAttributeEl1::Device_nGnRnE,
            MemoryAttributeEl1::Device_nGnRnE,
            MemoryAttributeEl1::Device_nGnRnE,
        ]);

        let wb_index = mair_el1
            .index_of(MemoryAttributeEl1::Normal_WriteBack)
            .expect("must be some WB memory available");

        let res = page_tables.map_pages(
            0,
            VirtualAddress::from(0),
            4,
            Arm64PageSize::Huge,
            wb_index,
            Arm64NoExecute::Full,
        );
        assert_eq!(res, Ok(()));
        assert_eq!(page_tables.lvl_stats(), [1, 1, 0, 0]);

        let res = page_tables.map_pages(
            1 << 30,
            VirtualAddress::from(0x4000_0000),
            4,
            Arm64PageSize::Small,
            wb_index,
            Arm64NoExecute::Full,
        );
        assert_eq!(res, Err(Arm64PageMapError::AlreadyMapped));
        assert_eq!(page_tables.lvl_stats(), [1, 1, 0, 0]);

        if DUMP_PAGE_TABLES {
            std::fs::write("page_tables_huge.bin", space).expect("can dump the page tables");
        }
    }

    #[test]
    fn test_mmu_page_mix() {
        let mut space = vec![0xaa; 0x100000];
        let mut page_tables = Arm64PageTableSpace::new(0x00000040248000, &mut space)
            .expect("Can initialize page tables");

        let mair_el1 = MemoryAttributeIndirectionEl1([
            MemoryAttributeEl1::Device_nGnRnE,
            MemoryAttributeEl1::Normal_NonCacheable,
            MemoryAttributeEl1::Normal_WriteThrough,
            MemoryAttributeEl1::Normal_WriteBack,
            MemoryAttributeEl1::Device_nGnRnE,
            MemoryAttributeEl1::Device_nGnRnE,
            MemoryAttributeEl1::Device_nGnRnE,
            MemoryAttributeEl1::Device_nGnRnE,
        ]);

        let wb_index = mair_el1
            .index_of(MemoryAttributeEl1::Normal_WriteBack)
            .expect("must be some WB memory available");

        const ONE_GIB: u64 = 1 << 30;

        let addr = ONE_GIB - 0x1000;
        let res = page_tables.map_range(
            addr,
            VirtualAddress::from(addr),
            3 * ONE_GIB,
            wb_index,
            Arm64NoExecute::Full,
        );
        assert_eq!(res, Ok(()));
        assert_eq!(page_tables.lvl_stats(), [1, 1, 2, 2]);
    }
}
