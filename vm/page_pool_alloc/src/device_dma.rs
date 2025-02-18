// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Module for device dma support.

// UNSAFETY: This is required to implement the MappedDmaTarget trait which
// unsafe because of it's requirement for the implementer to keep the
// `base()..len()` mapped for the lifetime of the struct.
#![expect(unsafe_code)]

use crate::PagePoolHandle;
use crate::PAGE_SIZE;
use user_driver::memory::MappedDmaTarget;

/// Page pool memory representing a DMA buffer useable by devices.
pub struct PagePoolDmaBuffer {
    // Holds allocation until dropped.
    pub(crate) alloc: PagePoolHandle,
    pub(crate) pfns: Vec<u64>,
}

/// SAFETY: This struct keeps both the shared memory region which the sparse
/// mapping maps, along with the sparse mapping itself until the struct is drop,
/// satisfying the trait.
unsafe impl MappedDmaTarget for PagePoolDmaBuffer {
    fn base(&self) -> *const u8 {
        self.alloc
            .inner
            .mapping
            .as_ptr()
            .wrapping_byte_add(self.alloc.mapping_offset)
            .cast()
    }

    fn len(&self) -> usize {
        (self.alloc.size_pages * PAGE_SIZE) as usize
    }

    fn pfns(&self) -> &[u64] {
        &self.pfns
    }

    fn pfn_bias(&self) -> u64 {
        self.alloc.inner.pfn_bias
    }
}
