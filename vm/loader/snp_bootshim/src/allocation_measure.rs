// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A test-only global allocator that measures how much the shim allocates.
//!
//! The shim allocates from a fixed-size boot heap, so tests use this to check
//! that the largest configuration fits.

#![expect(
    unsafe_code,
    reason = "test-only forwarding allocator measures bump-heap demand"
)]

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
