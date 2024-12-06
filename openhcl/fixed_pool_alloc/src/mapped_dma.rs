// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Module for device dma support at fixed location.

// UNSAFETY: This is required to implement the MappedDmaTarget trait which
// unsafe because of it's requirement for the implementer to keep the
// `base()..len()` mapped for the lifetime of the struct.
#![allow(unsafe_code)]

use crate::FixedPoolHandle;
use user_driver::memory::MappedDmaTarget;

/// DMA buffer at specific physical location.
pub struct FixedDmaBuffer {
    pub(crate) mapping: sparse_mmap::SparseMapping,
    // Holds allocation until dropped.
    pub(crate) _alloc: FixedPoolHandle,
    pub(crate) pfns: Vec<u64>,
}

/// SAFETY: This struct keeps both the fixed memory region which the sparse
/// mapping maps, along with the sparse mapping itself until the struct is dropped,
/// satisfying the trait.
unsafe impl MappedDmaTarget for FixedDmaBuffer {
    fn base(&self) -> *const u8 {
        self.mapping.as_ptr() as *const u8
    }

    fn len(&self) -> usize {
        self.mapping.len()
    }

    fn pfns(&self) -> &[u64] {
        &self.pfns
    }
}
