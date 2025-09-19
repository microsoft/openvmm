// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Local map and limited virtual address space manipulation support for the
//! bootshim.

//! Certain configurations of the bootshim need the ability to map in arbitrary
//! GPAs to process their contents in various ways. Additionally, certain VAs
//! need to be made host visible for certain periods of time. This module
//! provides the necessary support for manipulating the paging structures
//! involved.

use crate::single_threaded::SingleThreaded;
use core::arch::asm;
use core::cell::Cell;
use core::marker::PhantomData;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering;
use core::sync::atomic::compiler_fence;
use hvdef::HV_PAGE_SIZE;
use memory_range::MemoryRange;
use x86defs::X64_LARGE_PAGE_SIZE;
use x86defs::tdx::TDX_SHARED_GPA_BOUNDARY_ADDRESS_BIT;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

const X64_PTE_PRESENT: u64 = 1;
const X64_PTE_READ_WRITE: u64 = 1 << 1;
const X64_PTE_ACCESSED: u64 = 1 << 5;
const X64_PTE_DIRTY: u64 = 1 << 6;
const X64_PTE_LARGE_PAGE: u64 = 1 << 7;
const X64_PTE_CONFIDENTIAL: u64 = 1 << 51;

const PAGE_TABLE_ENTRY_COUNT: usize = 512;

const X64_PAGE_SHIFT: u64 = 12;
const X64_PTE_BITS: u64 = 9;

#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(transparent)]
struct PageTableEntry {
    entry: u64,
}
#[derive(Debug, Copy, Clone)]
pub enum PageTableEntryType {
    Leaf2MbPage(u64),
}

impl PageTableEntry {
    fn atomic_pte<'a>(&self) -> &'a AtomicU64 {
        // SAFETY: Casting a u64 to an atomic u64 via pointer is safe. All accesses to the u64 are
        // consistently performed using this method.
        unsafe {
            let ptr = &self.entry as *const u64;
            &*ptr.cast()
        }
    }

    fn write_pte(&mut self, val: u64) {
        self.atomic_pte().store(val, Ordering::SeqCst);
    }

    fn read_pte(&self) -> u64 {
        self.atomic_pte().load(Ordering::Relaxed)
    }

    /// Set an AMD64 PDE to either represent a leaf 2MB page or PDE.
    /// This sets the PTE to preset, accessed, dirty, read write execute.
    pub fn set_entry(&mut self, entry_type: PageTableEntryType, confidential: bool) {
        let mut entry: u64 = X64_PTE_PRESENT | X64_PTE_ACCESSED | X64_PTE_READ_WRITE;
        if confidential {
            entry |= X64_PTE_CONFIDENTIAL;
        }

        match entry_type {
            PageTableEntryType::Leaf2MbPage(address) => {
                // Leaf entry, set like UEFI does for 2MB pages. Must be 2MB aligned.
                assert!(address % X64_LARGE_PAGE_SIZE == 0);
                entry |= address;
                entry |= X64_PTE_LARGE_PAGE | X64_PTE_DIRTY;
            }
        }

        self.write_pte(entry);
    }

    pub fn is_present(&self) -> bool {
        self.read_pte() & X64_PTE_PRESENT == X64_PTE_PRESENT
    }

    pub fn is_large_page(&self) -> bool {
        self.entry & X64_PTE_LARGE_PAGE == X64_PTE_LARGE_PAGE
    }

    pub fn get_addr(&self) -> u64 {
        const VALID_BITS: u64 = 0x000f_ffff_ffff_f000;

        self.read_pte() & VALID_BITS & !X64_PTE_CONFIDENTIAL
    }

    pub fn clear(&mut self) {
        self.write_pte(0);
    }

    /// Check the TDX shared bit on a page table entry
    pub fn tdx_is_shared(&mut self) -> bool {
        let val = self.read_pte();
        val & TDX_SHARED_GPA_BOUNDARY_ADDRESS_BIT == TDX_SHARED_GPA_BOUNDARY_ADDRESS_BIT
    }

    /// Set the TDX shared bit on a page table entry
    pub fn tdx_set_shared(&mut self) {
        let mut val = self.read_pte();
        val |= TDX_SHARED_GPA_BOUNDARY_ADDRESS_BIT;
        self.write_pte(val);
    }

    /// Unset the TDX shared bit on a page table entry
    pub fn tdx_set_private(&mut self) {
        let mut val = self.read_pte();
        val &= !TDX_SHARED_GPA_BOUNDARY_ADDRESS_BIT;
        self.write_pte(val);
    }
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
struct PageTable {
    entries: [PageTableEntry; PAGE_TABLE_ENTRY_COUNT],
}

impl PageTable {
    /// Treat this page table as a page table of a given level, and locate the
    /// entry corresponding to a va.
    pub fn entry(&mut self, gva: u64, level: u8) -> &mut PageTableEntry {
        let index = get_amd64_pte_index(gva, level as u64) as usize;
        &mut self.entries[index]
    }
}

/// Get an AMD64 PTE index based on page table level.
fn get_amd64_pte_index(gva: u64, page_map_level: u64) -> u64 {
    let index = gva >> (X64_PAGE_SHIFT + page_map_level * X64_PTE_BITS);
    index & ((1 << X64_PTE_BITS) - 1)
}

/// Local Map. Provides a VA region where arbitrary physical addresses can be
/// mapped into the virtual address space on the current processor.
pub struct LocalMap<'a> {
    pte_ptr: *mut PageTableEntry,
    va: u64,
    _dummy: PhantomData<&'a ()>,
}

impl<'a> LocalMap<'a> {
    /// Maps in a contiguous page range into the local VA space.
    /// `range` specifies the address range to map.
    /// `confidential` indicates whether a confidential mapping is required.
    pub fn map_pages<'b>(
        &'b mut self,
        range: MemoryRange,
        confidential: bool,
    ) -> LocalMapMapping<'a, 'b> {
        let offset = range.start() % X64_LARGE_PAGE_SIZE;
        assert!(offset + range.len() <= X64_LARGE_PAGE_SIZE, "{range}");

        let aligned_gpa = range.start() - offset;
        let entry = self.local_map_entry();
        assert!(!entry.is_present());
        entry.set_entry(PageTableEntryType::Leaf2MbPage(aligned_gpa), confidential);
        let va = self.va + offset;
        // Prevent the compiler from moving any subsequent accesses to the local mapped pages to before
        // the mapping has actually been established in the page tables.
        compiler_fence(Ordering::SeqCst);
        // SAFETY: The va for the local map is part of the measured build. We've validated the range
        // is within bounds. We've checked that no entry is already present, so uniqueness is guaranteed.
        let buffer =
            unsafe { core::slice::from_raw_parts_mut(va as *mut u8, range.len() as usize) };
        LocalMapMapping {
            data: buffer,
            local_map: self,
        }
    }

    fn local_map_entry(&self) -> &'a mut PageTableEntry {
        // SAFETY: Called only once the local map has been initialized.
        unsafe { &mut *self.pte_ptr }
    }
}

pub struct LocalMapMapping<'a, 'b> {
    pub data: &'a mut [u8],
    local_map: &'b mut LocalMap<'a>,
}

impl Drop for LocalMapMapping<'_, '_> {
    fn drop(&mut self) {
        unmap_page_helper(self.local_map);
    }
}

fn unmap_page_helper(local_map: &LocalMap<'_>) {
    // All accesses to the local map must complete before clearing the PTE.
    compiler_fence(Ordering::SeqCst);
    // SAFETY: Clearing the stored local map pde and issuing invlpg, which is a benign instruction.
    // This routine must only be called once the local map has been initialized.
    unsafe {
        let entry = &mut *local_map.pte_ptr;
        entry.clear();
        let va = local_map.va;
        asm!("invlpg [{0}]", in(reg) va);
    }
}

/// Returns a reference to the page table page located at the specified physical
/// address.
///
/// # Safety
/// Caller ensures that the specified address is actually that of a page table.
unsafe fn page_table_at_address(address: u64) -> &'static mut PageTable {
    // SAFETY: Guaranteed by caller.
    unsafe { &mut *(address as *mut u64).cast() }
}

/// Returns a reference to the PDE corresponding to a virtual address.
///
/// # Safety
///
/// This routine requires the caller to ensure that the VA is a valid one for which the paging
/// hierarchy was configured by the file loader (the page directory must exist). If this is not
/// true this routine will panic rather than corrupt the address space.
unsafe fn get_pde_for_va(va: u64) -> &'static mut PageTableEntry {
    let mut page_table_base: u64;

    // SAFETY: See function comment.
    unsafe {
        asm!("mov {0}, cr3", out(reg) page_table_base);
        let pml4 = page_table_at_address(page_table_base);
        let entry = pml4.entry(va, 3);
        assert!(entry.is_present());
        let pdpt = page_table_at_address(entry.get_addr());
        let entry = pdpt.entry(va, 2);
        assert!(entry.is_present());
        let pd = page_table_at_address(entry.get_addr());
        pd.entry(va, 1)
    }
}

static LOCAL_MAP_INITIALIZED: SingleThreaded<Cell<bool>> = SingleThreaded(Cell::new(false));

/// Initializes the local map. This function should only be called once.
/// It returns a LocalMap structure with a static lifetime.
/// `va` is the virtual address of the local map region. It must be 2MB aligned.
pub fn init_local_map(va: u64) -> LocalMap<'static> {
    assert!(va.is_multiple_of(X64_LARGE_PAGE_SIZE));

    // SAFETY: The va for the local map is part of the measured build. This routine will only be
    // called once. The boot shim is a single threaded environment, the contained assertion is
    // sufficient to enforce that the routine is not called more than once.
    let local_map = unsafe {
        assert!(!LOCAL_MAP_INITIALIZED.get());
        LOCAL_MAP_INITIALIZED.set(true);
        let entry = get_pde_for_va(va);
        assert!(entry.is_present() && entry.is_large_page());

        LocalMap {
            pte_ptr: core::ptr::from_mut(entry),
            va,
            _dummy: PhantomData,
        }
    };

    unmap_page_helper(&local_map);
    local_map
}

/// A page used for TDX hypercalls
/// This wrapper assures that the page is a large page, present in the
/// paging hierarchy, aligned to 2MB, and shared with the hypervisor
pub struct TdxHypercallPage(u64);

impl TdxHypercallPage {
    /// Validate that a virtual address is present in the paging hierarchy,
    /// and that it is a large page
    ///
    /// # Safety
    /// The caller ensures that the input is a virtual address with a valid page table
    pub unsafe fn new(va: u64) -> Self {
        // SAFETY: Caller has guaranteed the va is a valid pagetable mapping
        unsafe {
            let entry = get_pde_for_va(va);
            assert!(entry.is_present() & entry.is_large_page());
            assert!(va.is_multiple_of(X64_LARGE_PAGE_SIZE));
            assert!(entry.tdx_is_shared());
            TdxHypercallPage(va)
        }
    }

    /// Returns the VA of the large page containing the I/O buffers
    pub fn base(&self) -> u64 {
        self.0
    }

    /// Returns the VA of the hypercall input buffer
    pub fn input(&self) -> u64 {
        self.0
    }

    /// Returns the VA of the hypercall output buffer
    pub fn output(&self) -> u64 {
        self.0 + HV_PAGE_SIZE
    }
}

/// Set the shared bit in the PDE of a large page in the local map for a given VA.
///
/// # Safety
/// The va passed in is guaranteed by the type to be a present large page,
/// the caller must ensure it is safe to share with the hypervisor
pub unsafe fn tdx_share_large_page(va: u64) {
    // SAFETY: See above
    unsafe {
        let entry = get_pde_for_va(va);
        entry.tdx_set_shared();
    }
}

/// Clear the shared bit in the PDE of the local map for a given VA.
pub fn tdx_unshare_large_page(va: TdxHypercallPage) {
    // SAFETY: The va passed in is guaranteed by the type to be a present large page,
    // which is shared with the hypervisor
    unsafe {
        let entry = get_pde_for_va(va.base());
        entry.tdx_set_private();
    }
}
