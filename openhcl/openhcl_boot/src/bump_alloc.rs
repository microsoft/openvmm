// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Simple bump allocator using https://os.phil-opp.com/allocator-designs/ as a
//! reference and starting point.
//!
//! Note that we only allow allocations in a small window for supporting
//! mesh_protobuf. Any other attempts to allocate will result in a panic.

use crate::boot_logger::log;
use crate::single_threaded::SingleThreaded;
use core::alloc::GlobalAlloc;
use core::alloc::Layout;
use core::cell::RefCell;
use memory_range::MemoryRange;

#[cfg_attr(minimal_rt, global_allocator)]
pub static ALLOCATOR: BumpAllocator = BumpAllocator::new();

#[derive(Debug)]
pub struct Inner {
    mem: MemoryRange,
    next: usize,
    allow_alloc: bool,
    alloc_count: usize,
}

pub struct BumpAllocator {
    inner: SingleThreaded<RefCell<Inner>>,
}

/// Align upwards. Returns the smallest x with alignment `align`
/// so that x >= addr. The alignment must be a power of 2.
pub fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

impl BumpAllocator {
    pub const fn new() -> Self {
        BumpAllocator {
            inner: SingleThreaded(RefCell::new(Inner {
                mem: MemoryRange::EMPTY,
                next: 0,
                allow_alloc: false,
                alloc_count: 0,
            })),
        }
    }

    /// Initialize the bump allocator with the specified memory range.
    ///
    /// # Safety
    /// The caller must guarantee that the memory range is both valid to
    /// access via the current pagetable identity map, and that it is unused.
    pub unsafe fn init(&self, mem: MemoryRange) {
        let mut inner = self.inner.borrow_mut();
        assert_eq!(
            inner.mem,
            MemoryRange::EMPTY,
            "bump allocator memory range previously set {}",
            inner.mem
        );

        inner.mem = mem;
        inner.next = mem.start() as usize;
    }

    pub fn enable_alloc(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.allow_alloc = true;
    }

    pub fn disable_alloc(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.allow_alloc = false;
    }

    pub fn log_stats(&self) {
        let inner = self.inner.borrow();
        log!(
            "Bump allocator: allocated {} bytes in {} allocations ({} bytes free)",
            inner.next - inner.mem.start() as usize,
            inner.alloc_count,
            inner.mem.end() as usize - inner.next
        );
    }
}

// SAFETY: The allocator points to a valid identity mapped memory range via
// init, which is unused by anything else.
unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut inner = self.inner.borrow_mut();

        if !inner.allow_alloc {
            panic!("allocations are not allowed");
        }

        let alloc_start: usize = align_up(inner.next, layout.align());

        let alloc_end = match alloc_start.checked_add(layout.size()) {
            Some(end) => end,
            None => return core::ptr::null_mut(),
        };

        log!(
            "bump_alloc: allocating {} bytes with alignment {} at {:#x}, alloc_end {:#x}",
            layout.size(),
            layout.align(),
            alloc_start,
            alloc_end,
        );

        if alloc_end > inner.mem.end() as usize {
            core::ptr::null_mut() // out of memory
        } else {
            inner.next = alloc_end;
            inner.alloc_count += 1;
            alloc_start as *mut u8
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_align_up() {
        assert_eq!(align_up(0x1000, 0x1000), 0x1000);
        assert_eq!(align_up(0x1001, 0x1000), 0x2000);
        assert_eq!(align_up(0x1FFF, 0x1000), 0x2000);
        assert_eq!(align_up(0x2000, 0x1000), 0x2000);

        assert_eq!(align_up(0x1003, 4), 0x1004);
        assert_eq!(align_up(0x1003, 8), 0x1008);
        assert_eq!(align_up(0x1003, 16), 0x1010);
    }

    #[test]
    fn test_alloc() {
        let allocator = BumpAllocator::new();
        // create a new page aligned box of memory with 20 pages.
        let mut buffer = Box::new([0; 0x1000 * 20]);
        let base_addr = buffer.as_ptr() as usize;
        // align up base_addr to the next page.
        let base_addr = align_up(base_addr as usize, 0x1000) as u64;
        assert_eq!(base_addr & 0xFFF, 0); // ensure page aligned

        let buffer_range = MemoryRange::new(base_addr..(base_addr + 10 * 0x1000));

        unsafe {
            allocator.init(buffer_range);
        }
        allocator.enable_alloc();

        unsafe {
            let ptr1 = allocator.alloc(Layout::from_size_align(100, 8).unwrap());
            *ptr1 = 42;
            assert_eq!(*ptr1, 42);

            let ptr2 = allocator.alloc(Layout::from_size_align(200, 16).unwrap());
            *ptr2 = 55;
            assert_eq!(*ptr2, 55);

            let ptr3 = allocator.alloc(Layout::from_size_align(300, 32).unwrap());
            *ptr3 = 77;
            assert_eq!(*ptr3, 77);
        }
    }
}
