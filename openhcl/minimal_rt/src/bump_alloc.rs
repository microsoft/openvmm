// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A simple bump allocator for single-threaded boot loaders.
//!
//! The allocator hands out memory from one identity-mapped range and never
//! frees it. Allocations are allowed only inside one call to
//! [`BumpAllocator::with_alloc`]. Any other allocation panics.

use core::alloc::GlobalAlloc;
use core::alloc::Layout;
use core::cell::RefCell;
use memory_range::MemoryRange;

#[derive(Debug, PartialEq, Eq)]
enum State {
    /// Allocations can be enabled via `enable_alloc`.
    Allowed,
    /// Allocations are currently enabled.
    Enabled,
    /// Allocations are disabled and cannot be enabled again.
    Disabled,
}

#[derive(Debug)]
struct Inner {
    start: *mut u8,
    next: *mut u8,
    end: *mut u8,
    allow_alloc: State,
    alloc_count: usize,
}

/// A bump allocator for a single-threaded boot loader.
///
/// Declare it as the `#[global_allocator]` of the boot loader, call
/// [`init`](Self::init) once, and then allocate inside
/// [`with_alloc`](Self::with_alloc).
///
/// The allocator is `Sync` so that it can be a global allocator, but it uses no
/// locks. [`new`](Self::new) is therefore unsafe.
pub struct BumpAllocator {
    inner: RefCell<Inner>,
}

// SAFETY: The caller of `new` guarantees that only one thread uses the
// allocator, so no other thread can access the `RefCell`.
unsafe impl Sync for BumpAllocator {}

/// Usage statistics for a [`BumpAllocator`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// The number of bytes allocated, including alignment padding.
    pub allocated: usize,
    /// The number of bytes still free.
    pub free: usize,
    /// The number of allocations.
    pub count: usize,
}

impl BumpAllocator {
    /// Creates an allocator with no memory.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that only one thread ever uses the allocator.
    pub const unsafe fn new() -> Self {
        BumpAllocator {
            inner: RefCell::new(Inner {
                start: core::ptr::null_mut(),
                next: core::ptr::null_mut(),
                end: core::ptr::null_mut(),
                allow_alloc: State::Allowed,
                alloc_count: 0,
            }),
        }
    }

    /// Initialize the bump allocator with the specified memory range.
    ///
    /// Panics if the allocator was already initialized.
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

    /// Runs `f` with allocations enabled.
    ///
    /// Panics if allocations were enabled before. Allocations stay disabled
    /// after `f` returns.
    pub fn with_alloc<T>(&self, f: impl FnOnce() -> T) -> T {
        self.enable_alloc();
        let val = f();
        self.disable_alloc();
        val
    }

    /// Returns the usage statistics of the allocator.
    pub fn stats(&self) -> Stats {
        let inner = self.inner.borrow();
        Stats {
            allocated: inner.next as usize - inner.start as usize,
            free: inner.end as usize - inner.next as usize,
            count: inner.alloc_count,
        }
    }

    /// Enable allocations. This panics if allocations were ever previously
    /// enabled.
    fn enable_alloc(&self) {
        let mut inner = self.inner.borrow_mut();

        inner.allow_alloc = match inner.allow_alloc {
            State::Allowed => State::Enabled,
            State::Enabled => {
                panic!("allocations are already enabled");
            }
            State::Disabled => {
                panic!("allocations were previously disabled and cannot be re-enabled");
            }
        };
    }

    /// Disable allocations. Panics if the allocator was not previously enabled.
    fn disable_alloc(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.allow_alloc = match inner.allow_alloc {
            State::Allowed => panic!("allocations were never enabled"),
            State::Enabled => State::Disabled,
            State::Disabled => {
                panic!("allocations were previously disabled and cannot be disabled again");
            }
        };
    }
}

// SAFETY: The allocator points to a valid identity VA range via the
// construction at init.
unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut inner = self.inner.borrow_mut();

        if inner.allow_alloc != State::Enabled {
            panic!("allocations are not allowed {:?}", inner.allow_alloc);
        }

        let align_offset = inner.next.align_offset(layout.align());
        let alloc_start = inner.next.wrapping_add(align_offset);
        let alloc_end = alloc_start.wrapping_add(layout.size());

        // If end overflowed this allocation is too large. If start overflowed,
        // end will also overflow.
        //
        // Rust `Layout` guarantees that the size is not larger than `isize`,
        // so it's not possible to wrap around twice.
        if alloc_end < alloc_start {
            return core::ptr::null_mut();
        }

        if alloc_end > inner.end {
            core::ptr::null_mut() // out of memory
        } else {
            inner.next = alloc_end;
            inner.alloc_count += 1;
            alloc_start
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}

    // TODO: consider implementing realloc for the Vec grow case, which is the
    // main usecase we see. This would mean supporting realloc if the allocation
    // being realloced was the last one aka the tail.
}

#[cfg(nightly)]
// SAFETY: The allocator points to a valid identity VA range via the
// construction at init, the same as for `GlobalAlloc`.
unsafe impl core::alloc::Allocator for &BumpAllocator {
    fn allocate(
        &self,
        layout: Layout,
    ) -> Result<core::ptr::NonNull<[u8]>, core::alloc::AllocError> {
        // SAFETY: The caller meets the `GlobalAlloc::alloc` contract.
        let ptr = unsafe { self.alloc(layout) };
        core::ptr::NonNull::new(ptr)
            .map(|ptr| core::ptr::NonNull::slice_from_raw_parts(ptr, layout.size()))
            .ok_or(core::alloc::AllocError)
    }

    unsafe fn deallocate(&self, _ptr: core::ptr::NonNull<u8>, _layout: Layout) {}
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    const SIZE: usize = 0x1000 * 20;

    fn layout(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).unwrap()
    }

    /// A page-aligned host buffer, because `MemoryRange` must be page-aligned.
    struct Buffer(*mut u8);

    impl Buffer {
        fn new() -> Self {
            // SAFETY: The layout has a nonzero size.
            let ptr = unsafe { std::alloc::alloc(layout(SIZE, 0x1000)) };
            assert!(!ptr.is_null());
            Self(ptr)
        }

        fn allocator(&self) -> BumpAllocator {
            // SAFETY: Only this test's thread uses the allocator.
            let allocator = unsafe { BumpAllocator::new() };
            let start = self.0 as u64;
            // SAFETY: The buffer is valid and unused, and it outlives the
            // allocator in each test.
            unsafe { allocator.init(MemoryRange::new(start..start + SIZE as u64)) };
            allocator
        }
    }

    impl Drop for Buffer {
        fn drop(&mut self) {
            // SAFETY: The buffer came from `std::alloc::alloc` with this layout.
            unsafe { std::alloc::dealloc(self.0, layout(SIZE, 0x1000)) };
        }
    }

    #[test]
    fn allocates_aligned_memory_until_full() {
        let buffer = Buffer::new();
        let allocator = buffer.allocator();
        allocator.with_alloc(|| {
            for (size, align) in [(100, 8), (200, 16), (300, 32)] {
                // SAFETY: The layout has a nonzero size.
                let ptr = unsafe { allocator.alloc(layout(size, align)) };
                assert!(!ptr.is_null());
                assert_eq!(ptr as usize % align, 0);
                // SAFETY: The allocation is within the buffer.
                unsafe { ptr.write(42) };
            }
            // SAFETY: The layout has a nonzero size.
            let too_large = unsafe { allocator.alloc(layout(SIZE, 8)) };
            assert!(too_large.is_null());
        });

        let stats = allocator.stats();
        assert_eq!(stats.count, 3);
        assert_eq!(stats.allocated + stats.free, SIZE);
        assert!(stats.allocated >= 600);
    }

    // NOTE: run this test with miri via
    // `RUSTFLAGS="--cfg nightly" cargo +nightly miri test -p minimal_rt`
    #[cfg(nightly)]
    #[test]
    fn vec_grows_in_the_allocator() {
        let buffer = Buffer::new();
        let allocator = buffer.allocator();
        allocator.with_alloc(|| {
            let mut vec: std::vec::Vec<u8, &BumpAllocator> = std::vec::Vec::new_in(&allocator);
            // Push 4096 bytes, which forces the vec to reallocate.
            for i in 0..4096 {
                vec.push(i as u8);
            }
            vec.resize(10000, 0);
        });
    }

    #[test]
    #[should_panic(expected = "allocations are not allowed")]
    fn allocation_outside_the_window_panics() {
        let buffer = Buffer::new();
        let allocator = buffer.allocator();
        allocator.with_alloc(|| ());
        // SAFETY: The layout has a nonzero size.
        unsafe { allocator.alloc(layout(8, 8)) };
    }
}
