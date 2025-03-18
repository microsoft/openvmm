// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This does something

use guestmem::GuestMemory;
use guestmem::GuestMemoryAccess;
use parking_lot::Mutex;
use std::ptr::NonNull;
use std::sync::Arc;
use user_driver::memory::MappedDmaTarget;
use user_driver::memory::PAGE_SIZE;

/// The [`GuestMemoryAccessWrapper`] struct is meant for testing only. It is meant to encapsulate types that already
/// implement [`GuestMemoryAccess`] but provides the allow_dma switch regardless of the underlying
/// type T.
pub struct GuestMemoryAccessWrapper<T> {
    mem: T,
    allow_dma: bool,
}

impl<T> GuestMemoryAccessWrapper<T> {
    /// Creates and returns a new [`GuestMemoryAccessWrapper`] with given memory and the allow_dma switch.
    /// `mem` must implement the [`GuestMemoryAccess`] trait.
    pub fn new(mem: T, allow_dma: bool) -> Self {
        Self { mem, allow_dma}
    }

    /// Returns a ref to underlying `mem`
    pub fn mem(&self) -> &T {
        &self.mem
    }
}

/// SAFETY: Defer to [`GuestMemoryAccess`] implementation of T
/// Only intercept the base_iova fn with a naive response of 0 if allow_dma is enabled.
unsafe impl<T: GuestMemoryAccess> GuestMemoryAccess for GuestMemoryAccessWrapper<T> {
    fn mapping(&self) -> Option<NonNull<u8>> {
        self.mem.mapping()
    }

    fn base_iova(&self) -> Option<u64> {
        self.allow_dma.then_some(0)
    }

    fn max_address(&self) -> u64 {
        self.mem.max_address()
    }
}

impl<T: GuestMemoryAccess> GuestMemoryAccessWrapper<T> {
    /// Takes sparse mapping as input and converts it to [`GuestMemory`] with the allow_dma switch
    pub fn create_test_guest_memory(mem: T, allow_dma: bool) -> GuestMemory {
        let test_backing = GuestMemoryAccessWrapper { mem, allow_dma };
        GuestMemory::new("test mapper guest memory", test_backing)
    }
}

/// DmaBuffer struct
pub struct DmaBuffer {
    mem: GuestMemory,
    pfns: Vec<u64>,
    state: Arc<Mutex<Vec<u64>>>,
}

impl DmaBuffer {
    /// Creates and returns new [`DmaBuffer`] with the given input parameters
    pub fn new(mem: GuestMemory, pfns: Vec<u64>, state: Arc<Mutex<Vec<u64>>>) -> Self {
        Self {
            mem,
            pfns,
            state,
        }
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
