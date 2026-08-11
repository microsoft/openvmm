// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements the [`MappedDmaTarget`] trait for a wrapped [`MemoryBlock`]
//! returned by [`LowerVtlMemorySpawner`].

// UNSAFETY: No unsafe code here, but required for implementing MappedDmaTarget.
// The implementation is just forwarding the calls to the underlying wrapped
// MemoryBlock.
#![expect(unsafe_code)]

use crate::PagesAccessibleToLowerVtl;
use inspect::Inspect;
use user_driver::memory::MappedDmaTarget;
use user_driver::memory::MemoryBlock;

/// A DMA buffer where permissions of the pages have been lowered to allow
/// access to all VTLs.
#[derive(Inspect)]
pub struct LowerVtlDmaBuffer {
    // NOTE: This must be dropped first to restore VTL protections to the
    // underlying pages before we free the MemoryBlock and return any pages to
    // be allocated again.
    pub(crate) _vtl_guard: PagesAccessibleToLowerVtl,
    #[inspect(skip)]
    pub(crate) block: MemoryBlock,
}

// SAFETY: The underlying MemoryBlock is providing the implementation for this
// trait.
unsafe impl MappedDmaTarget for LowerVtlDmaBuffer {
    fn base(&self) -> *const u8 {
        self.block.base()
    }

    fn len(&self) -> usize {
        self.block.len()
    }

    fn pfns(&self) -> &[u64] {
        self.block.pfns()
    }

    fn pfn_bias(&self) -> u64 {
        self.block.pfn_bias()
    }
}
