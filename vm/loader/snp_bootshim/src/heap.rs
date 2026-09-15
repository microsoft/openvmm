// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Single-threaded, bounded allocation during the measured bootshim phase.

use core::alloc::Layout;

#[derive(Debug)]
struct Cursor {
    #[cfg(test)]
    start: usize,
    next: usize,
    end: usize,
}

impl Cursor {
    fn new(start: usize, size: usize) -> Option<Self> {
        Some(Self {
            #[cfg(test)]
            start,
            next: start,
            end: start.checked_add(size)?,
        })
    }

    fn allocate(&mut self, layout: Layout) -> Option<usize> {
        let aligned = self.next.checked_add(layout.align() - 1)? & !(layout.align() - 1);
        let end = aligned.checked_add(layout.size().max(1))?;
        if end > self.end {
            return None;
        }
        self.next = end;
        Some(aligned)
    }

    #[cfg(test)]
    fn used(&self) -> usize {
        self.next - self.start
    }
}

#[cfg(minimal_rt)]
mod runtime {
    use super::*;
    use core::alloc::GlobalAlloc;
    use core::cell::UnsafeCell;

    pub struct BootHeap(UnsafeCell<Option<Cursor>>);

    // SAFETY: Only the BSP executes the bootshim. No allocation is permitted
    // before initialization or after control transfers to Linux.
    unsafe impl Sync for BootHeap {}

    #[global_allocator]
    pub static HEAP: BootHeap = BootHeap(UnsafeCell::new(None));

    impl BootHeap {
        /// # Safety
        /// The region must be accepted, identity-mapped, writable, and disjoint
        /// from every live image/parameter/output region. Initialize once.
        pub unsafe fn init(&self, start: usize, size: usize) -> bool {
            // SAFETY: Initialization occurs once on the only executing CPU.
            let state = unsafe { &mut *self.0.get() };
            if state.is_some() {
                return false;
            }
            *state = Cursor::new(start, size);
            state.is_some()
        }
    }

    // SAFETY: The measured handoff supplies an exclusive, accepted region.
    // All allocations are aligned and checked against its end. Memory is
    // intentionally not reclaimed during this bounded, one-shot boot phase.
    unsafe impl GlobalAlloc for BootHeap {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: The bootshim runs only on the BSP, without reentrancy.
            let state = unsafe { &mut *self.0.get() };
            match state.as_mut().and_then(|cursor| cursor.allocate(layout)) {
                Some(address) => address as *mut u8,
                None => crate::terminate(),
            }
        }

        unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
    }
}

#[cfg(minimal_rt)]
pub use runtime::HEAP;

#[cfg(test)]
#[expect(
    unsafe_code,
    reason = "test-only forwarding allocator measures bump-heap demand"
)]
pub(crate) mod allocation_measure {
    use std::alloc::GlobalAlloc;
    use std::alloc::Layout;
    use std::alloc::System;
    use std::cell::Cell;

    std::thread_local! {
        static ALLOCATED: Cell<Option<usize>> = const { Cell::new(None) };
    }

    struct Measure;

    #[global_allocator]
    static ALLOCATOR: Measure = Measure;

    fn count(size: usize, alignment: usize) {
        let _ = ALLOCATED.try_with(|allocated| {
            if let Some(total) = allocated.get() {
                allocated.set(Some(total + size + alignment - 1));
            }
        });
    }

    // SAFETY: All operations are forwarded unchanged to System. The
    // thread-local counter performs no allocations and tracks only this test.
    unsafe impl GlobalAlloc for Measure {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            count(layout.size(), layout.align());
            // SAFETY: The caller's layout is forwarded unchanged.
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            count(layout.size(), layout.align());
            // SAFETY: The caller's layout is forwarded unchanged.
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            count(size, layout.align());
            // SAFETY: The pointer and layout belong to this System allocator.
            unsafe { System.realloc(ptr, layout, size) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: The pointer and layout belong to this System allocator.
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    pub fn measure<T>(f: impl FnOnce() -> T) -> (T, usize) {
        ALLOCATED.with(|allocated| assert!(allocated.replace(Some(0)).is_none()));
        let result = f();
        let allocated = ALLOCATED.with(|allocated| allocated.replace(None).unwrap());
        (result, allocated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn bounded_cursor_accounts_for_alignment_and_exhaustion() {
        let mut cursor = Cursor::new(0x1000, 64).unwrap();
        assert_eq!(
            cursor.allocate(Layout::from_size_align(1, 1).unwrap()),
            Some(0x1000)
        );
        assert_eq!(
            cursor.allocate(Layout::from_size_align(16, 16).unwrap()),
            Some(0x1010)
        );
        assert_eq!(cursor.used(), 32);
        assert_eq!(
            cursor.allocate(Layout::from_size_align(32, 16).unwrap()),
            Some(0x1020)
        );
        assert_eq!(
            cursor.allocate(Layout::from_size_align(1, 1).unwrap()),
            None
        );
        assert_eq!(cursor.used(), 64);
    }

    #[test]
    fn bounded_cursor_rejects_overflow() {
        assert!(Cursor::new(usize::MAX, 1).is_none());
        let mut cursor = Cursor::new(usize::MAX - 2, 2).unwrap();
        assert_eq!(
            cursor.allocate(Layout::from_size_align(1, 4).unwrap()),
            None
        );
    }
}
