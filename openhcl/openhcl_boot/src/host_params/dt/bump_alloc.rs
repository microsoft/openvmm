// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A simple bump allocator that can be used in the bootloader.
//!
//! Note that we only allow allocations in a small window for supporting
//! mesh_protobuf. Any other attempts to allocate will result in a panic.

use crate::single_threaded::SingleThreaded;
use core::alloc::GlobalAlloc;
use core::alloc::Layout;
use core::cell::RefCell;
use memory_range::MemoryRange;

// Only enable the bump allocator when compiling with minimal_rt, as otherwise
// it will override the global allocator in unit tests which is not what we
// want.
#[cfg_attr(minimal_rt, global_allocator)]
pub static ALLOCATOR: BumpAllocator = BumpAllocator::new();

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
pub struct Inner {
    start: *mut u8,
    next: *mut u8,
    end: *mut u8,
    allow_alloc: State,
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
                allow_alloc: State::Allowed,
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

    fn log_stats(&self) {
        let inner = self.inner.borrow();

        // SAFETY: The pointers are within the same original allocation,
        // specified by init. They are u8 pointers, so there is no alignment
        // requirement.
        let (allocated, free) = unsafe {
            (
                inner.next.offset_from(inner.start),
                inner.end.offset_from(inner.next),
            )
        };

        log::info!(
            "Bump allocator: allocated {} bytes in {} allocations ({} bytes free)",
            allocated,
            inner.alloc_count,
            free
        );
    }
}

/// Run the provided closure with enabling the global bump allocator. This is
/// only intended to be used for mesh_protobuf decode.
///
/// Note that if the global allocator was ever used before, this function will
/// panic.
pub fn with_global_alloc<T>(f: impl FnOnce() -> T) -> T {
    ALLOCATOR.enable_alloc();
    let val = f();
    ALLOCATOR.disable_alloc();
    ALLOCATOR.log_stats();
    val
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

        // TODO: renable allocation tracing when we support tracing levels via
        // the log crate.

        if alloc_end > inner.end {
            core::ptr::null_mut() // out of memory
        } else {
            inner.next = alloc_end;
            inner.alloc_count += 1;
            alloc_start
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // TODO: renable allocation tracing when we support tracing levels via
        // the log crate.
    }

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
        let ptr = unsafe { self.alloc(layout) };
        if ptr.is_null() {
            Err(core::alloc::AllocError)
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

#[cfg(nightly)]
#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: run these tests with miri via
    // `RUSTFLAGS="--cfg nightly" cargo +nightly miri test -p openhcl_boot`
    #[test]
    fn test_alloc() {
        let buffer: Box<[u8]> = Box::new([0; 0x1000 * 20]);
        let addr = Box::into_raw(buffer) as *mut u8;
        let allocator = BumpAllocator {
            inner: SingleThreaded(RefCell::new(Inner {
                start: addr,
                next: addr,
                end: unsafe { addr.add(0x1000 * 20) },
                allow_alloc: State::Allowed,
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
        }

        {
            let mut vec: Vec<u8, &BumpAllocator> = Vec::new_in(&allocator);

            // Push 4096 bytes, which should force a vec realloc.
            for i in 0..4096 {
                vec.push(i as u8);
            }

            // force an explicit resize to 10000 bytes
            vec.resize(10000, 0);
        }

        // Attempt to allocate a large chunk that is not available.
        unsafe {
            let ptr4 = allocator.alloc(Layout::from_size_align(0x1000 * 20, 8).unwrap());
            assert!(ptr4.is_null());
        }

        // Recreate the box, then drop it so miri is satisfied.
        let _buf = unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(addr, 0x1000 * 20)) };

        allocator.log_stats();
    }
}
