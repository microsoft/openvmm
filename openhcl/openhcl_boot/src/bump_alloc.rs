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
    start: *mut u8,
    next: *mut u8,
    end: *mut u8,
    allow_alloc: bool,
    alloc_count: usize,
}

pub struct BumpAllocator {
    inner: SingleThreaded<RefCell<Inner>>,
}

impl BumpAllocator {
    pub const fn new() -> Self {
        BumpAllocator {
            inner: SingleThreaded(RefCell::new(Inner {
                start: core::ptr::null_mut(),
                next: core::ptr::null_mut(),
                end: core::ptr::null_mut(),
                allow_alloc: false,
                alloc_count: 0,
            })),
        }
    }

    /// Initialize the bump allocator with the specified memory range.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that the memory range is both valid to
    /// access via the current pagetable identity map, and that it is unused.
    pub unsafe fn init(&self, mem: MemoryRange) {
        let mut inner = self.inner.borrow_mut();
        assert!(
            inner.start.is_null(),
            "bump allocator memory range previously set {:#x?}",
            inner.start
        );

        inner.start = mem.start() as *mut u8;
        inner.next = mem.start() as *mut u8;
        inner.end = mem.end() as *mut u8;
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

        // FIXME: unsafe calcs
        let allocated = unsafe { inner.next.offset_from(inner.start) };
        let free = unsafe { inner.end.offset_from(inner.next) };
        log!(
            "Bump allocator: allocated {} bytes in {} allocations ({} bytes free)",
            allocated,
            inner.alloc_count,
            free
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

        // FIXME: verify math and wraparounds
        let align_offset = inner.next.align_offset(layout.align());
        let alloc_start = inner.next.wrapping_add(align_offset);
        let alloc_end = alloc_start.wrapping_add(layout.size());

        if alloc_end < alloc_start {
            // overflow
            return core::ptr::null_mut();
        }

        log!(
            "bump_alloc: allocating {} bytes with alignment {} at with offset {} alloc_start {:#x?}, alloc_end {:#x?}",
            layout.size(),
            layout.align(),
            align_offset,
            alloc_start,
            alloc_end,
        );

        if alloc_end > inner.end {
            core::ptr::null_mut() // out of memory
        } else {
            inner.next = alloc_end;
            inner.alloc_count += 1;
            alloc_start
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        log!("dealloc called on {:#x?} of size {}", ptr, layout.size());
    }
}

#[cfg(feature = "nightly")]
unsafe impl core::alloc::Allocator for BumpAllocator {
    fn allocate(&self, layout: Layout) -> Result<core::ptr::NonNull<[u8]>, std::alloc::AllocError> {
        let ptr = unsafe { self.alloc(layout) };
        if ptr.is_null() {
            Err(std::alloc::AllocError)
        } else {
            unsafe {
                Ok(core::ptr::NonNull::slice_from_raw_parts(
                    core::ptr::NonNull::new_unchecked(ptr),
                    layout.size(),
                ))
            }
        }
    }

    unsafe fn deallocate(&self, ptr: core::ptr::NonNull<u8>, layout: Layout) {
        log!("deallocate called on {:#x?} of size {}", ptr, layout.size());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "nightly")]
    #[test]
    fn test_alloc() {
        let buffer: Box<[u8]> = Box::new([0; 0x1000 * 20]);
        let addr = Box::into_raw(buffer) as *mut u8;
        let allocator = BumpAllocator {
            inner: SingleThreaded(RefCell::new(Inner {
                start: addr,
                next: addr,
                end: unsafe { addr.add(0x1000 * 20) },
                allow_alloc: false,
                alloc_count: 0,
            })),
        };
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

            let mut vec: Vec<u8, BumpAllocator> = Vec::new_in(allocator);

            // Push 4096 bytes, which should force a vec realloc.
            for i in 0..4096 {
                vec.push(i as u8);
            }

            // force an explicit resize to 10000 bytes
            vec.resize(10000, 0);
        }

        // Recreate the box, then drop it so miri is satisifed.
        let _buf = unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(addr, 0x1000 * 20)) };
    }
}
