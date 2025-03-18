// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module defines and provides an unsafe [`GuestMemoryAccess`] trait implementation for [`GuestMemoryAccessWrapper`].
//! Refer to to struct comment for more details.

// UNSAFETY: underlying struct needs to implement the unsafe [`GuestMemoryAccess`] trait.
#![expect(unsafe_code)]

use guestmem::GuestMemory;
use guestmem::GuestMemoryAccess;
use std::ptr::NonNull;

/// The [`GuestMemoryAccessWrapper`] encapsulates types T that already implement [`GuestMemoryAccess`].
/// It provides the allow_dma switch regardless of the underlying type T.
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
