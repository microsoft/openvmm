// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Simple bump allocator using https://os.phil-opp.com/allocator-designs/ as a
//! reference and starting point.
//!
//! Note that we only allow allocations in a small window for supporting
//! mesh_protobuf. Any other attempts to allocate will result in a panic.

use crate::boot_logger::debug_log;
use crate::single_threaded::SingleThreaded;
use core::alloc::GlobalAlloc;
use core::alloc::Layout;
use core::cell::RefCell;
use memory_range::MemoryRange;

#[global_allocator]
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

/// Align downwards. Returns the greatest x with alignment `align`
/// so that x <= addr. The alignment must be a power of 2.
pub fn align_down(addr: usize, align: usize) -> usize {
    if align.is_power_of_two() {
        addr & !(align - 1)
    } else if align == 0 {
        addr
    } else {
        panic!("`align` must be a power of 2");
    }
}

/// Align upwards. Returns the smallest x with alignment `align`
/// so that x >= addr. The alignment must be a power of 2.
pub fn align_up(addr: usize, align: usize) -> usize {
    align_down(addr + align - 1, align)
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
}

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
