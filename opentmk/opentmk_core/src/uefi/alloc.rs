// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::alloc::GlobalAlloc;
use core::cell::RefCell;
use core::ptr::NonNull;

use linked_list_allocator::LockedHeap;
use spin::Mutex;
use uefi::allocator::Allocator;
use uefi::boot::AllocateType;
use uefi::boot::MemoryType;
use uefi::boot::{self};

/// The number of bytes in one mebibyte.
pub const SIZE_1MB: usize = 1024 * 1024;
const PAGE_SIZE: usize = 4096;

/// The global allocator used by the UEFI runtime.
#[global_allocator]
pub static ALLOCATOR: MemoryAllocator = MemoryAllocator {
    use_locked_heap: Mutex::new(RefCell::new(false)),
    locked_heap: LockedHeap::empty(),
    uefi_allocator: Allocator {},
};

/// An allocator that can switch from UEFI boot services to a fixed-size heap.
pub struct MemoryAllocator {
    use_locked_heap: Mutex<RefCell<bool>>,
    locked_heap: LockedHeap,
    uefi_allocator: Allocator,
}

// SAFETY: The methods of GlobalAlloc are unsafe because the caller must ensure the safety
unsafe impl GlobalAlloc for MemoryAllocator {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        // SAFETY: caller must ensure layout is valid
        unsafe { self.get_allocator().alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
        // SAFETY: caller must ensure ptr and layout are valid
        unsafe { self.get_allocator().dealloc(ptr, layout) };
    }

    unsafe fn alloc_zeroed(&self, layout: core::alloc::Layout) -> *mut u8 {
        // SAFETY: caller must ensure layout is valid
        unsafe { self.get_allocator().alloc_zeroed(layout) }
    }

    unsafe fn realloc(
        &self,
        ptr: *mut u8,
        layout: core::alloc::Layout,
        new_size: usize,
    ) -> *mut u8 {
        // SAFETY: caller must ensure ptr is valid for layout
        unsafe { self.get_allocator().realloc(ptr, layout, new_size) }
    }
}

impl MemoryAllocator {
    /// Allocates `num_pages` pages of UEFI loader-data memory.
    pub fn allocate_pages_uefi(
        &self,
        ty: AllocateType,
        num_pages: usize,
    ) -> Result<NonNull<u8>, uefi::Error> {
        boot::allocate_pages(ty, MemoryType::LOADER_DATA, num_pages)
    }

    /// Switches allocation to a locked heap of at least `size` MiB.
    ///
    /// Returns `false` if UEFI boot services cannot allocate the heap.
    pub fn switch_to_capped_heap(&self, size: usize) -> bool {
        let pages = ((SIZE_1MB * size) / 4096) + 1;
        let size = pages * 4096;
        let mem: Result<NonNull<u8>, uefi::Error> = boot::allocate_pages(
            AllocateType::AnyPages,
            MemoryType::BOOT_SERVICES_DATA,
            pages,
        );
        if mem.is_err() {
            return false;
        }
        let ptr = mem.unwrap().as_ptr();
        // SAFETY: its safe to init a locked heap at this point, we know memory allocated is valid
        unsafe { self.locked_heap.lock().init(ptr, size) };
        *self.use_locked_heap.lock().borrow_mut() = true;
        true
    }

    /// Allocates page-aligned memory with capacity for at least `size` MiB.
    ///
    /// Returns a null pointer if UEFI boot services cannot allocate the memory.
    pub fn get_page_aligned_memory_uefi(&self, size: usize) -> *mut u8 {
        let pages = ((SIZE_1MB * size) / PAGE_SIZE) + 1;
        let mem: Result<NonNull<u8>, uefi::Error> = boot::allocate_pages(
            AllocateType::AnyPages,
            MemoryType::BOOT_SERVICES_DATA,
            pages,
        );
        if mem.is_err() {
            return core::ptr::null_mut();
        }
        mem.unwrap().as_ptr()
    }

    fn get_allocator(&self) -> &dyn GlobalAlloc {
        if *self.use_locked_heap.lock().borrow() {
            &self.locked_heap
        } else {
            &self.uefi_allocator
        }
    }
}
