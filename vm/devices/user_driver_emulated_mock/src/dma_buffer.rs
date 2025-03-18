// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module defines and provides an unsafe [`MappedDmaTarget`] implementation for [`DmaBuffer`]. Refer to to struct comment for details.

// UNSAFETY: underlying struct needs to implement the unsafe [`MappedDmaTarget`] trait.
#![expect(unsafe_code)]

use guestmem::GuestMemory;
use parking_lot::Mutex;
use std::sync::Arc;
use user_driver::memory::MappedDmaTarget;
use user_driver::memory::PAGE_SIZE;

/// A representation of a contiguous slice of memory in a larger [`GuestMemory`]
pub struct DmaBuffer {
    mem: GuestMemory,
    pfns: Vec<u64>,
    state: Arc<Mutex<Vec<u64>>>,
}

impl DmaBuffer {
    /// Creates and returns new [`DmaBuffer`] with the given input parameters
    pub fn new(mem: GuestMemory, pfns: Vec<u64>, state: Arc<Mutex<Vec<u64>>>) -> Self {
        Self { mem, pfns, state }
    }
}

impl Drop for DmaBuffer {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        for &pfn in &self.pfns {
            state[pfn as usize / 64] &= !(1 << (pfn % 64));
        }
    }
}

/// SAFETY: we are handing out a VA and length for valid data, propagating the
/// guarantee from [`GuestMemory`] (which is known to be in a fully allocated
/// state because we used `GuestMemory::allocate` to create it).
unsafe impl MappedDmaTarget for DmaBuffer {
    fn base(&self) -> *const u8 {
        self.mem
            .full_mapping()
            .unwrap()
            .0
            .wrapping_add(self.pfns[0] as usize * PAGE_SIZE)
    }

    fn len(&self) -> usize {
        self.pfns.len() * PAGE_SIZE
    }

    fn pfns(&self) -> &[u64] {
        &self.pfns
    }

    fn pfn_bias(&self) -> u64 {
        0
    }
}
