// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The boot loader's global bump allocator.
//!
//! Allocations are allowed only in a small window for supporting
//! mesh_protobuf. Any other attempt to allocate results in a panic.

use minimal_rt::bump_alloc::BumpAllocator;

// Only enable the bump allocator when compiling with minimal_rt, as otherwise
// it will override the global allocator in unit tests which is not what we
// want.
#[cfg_attr(minimal_rt, global_allocator)]
// SAFETY: The boot loader runs on one thread only.
pub static ALLOCATOR: BumpAllocator = unsafe { BumpAllocator::new() };

/// Run the provided closure with enabling the global bump allocator. This is
/// only intended to be used for mesh_protobuf decode.
///
/// Note that if the global allocator was ever used before, this function will
/// panic.
pub fn with_global_alloc<T>(f: impl FnOnce() -> T) -> T {
    let val = ALLOCATOR.with_alloc(f);
    let stats = ALLOCATOR.stats();
    log::info!(
        "Bump allocator: allocated {} bytes in {} allocations ({} bytes free)",
        stats.allocated,
        stats.count,
        stats.free
    );
    val
}
