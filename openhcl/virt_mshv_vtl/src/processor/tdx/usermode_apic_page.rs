// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: Transmuting a raw page mapping to a typed reference.
#![expect(unsafe_code)]

use hcl::GuestVtl;
use page_pool_alloc::PagePoolAllocator;
use page_pool_alloc::PagePoolHandle;
use x86defs::vmx::ApicPage;

pub(super) struct UsermodeApicPage(PagePoolHandle);

impl UsermodeApicPage {
    pub fn new(pool: &PagePoolAllocator, vtl: GuestVtl) -> Result<Self, crate::Error> {
        let handle = pool
            .alloc_with_mapping(1.try_into().unwrap(), format!("tdx_{:?}_apic_page", vtl))
            .map_err(crate::Error::AllocatePrivatePages)?;
        Ok(Self(handle))
    }

    pub fn get(&self) -> &ApicPage {
        // SAFETY: We know that the the page will never be accessed as any other
        // type, and that the page is always mapped.
        unsafe { &*self.0.mapping().unwrap().as_ptr().cast() }
    }

    pub fn get_mut(&mut self) -> &mut ApicPage {
        // SAFETY: We know that the the page will never be accessed as any other
        // type, and that the page is always mapped.
        unsafe { &mut *self.0.mapping().unwrap().as_ptr().cast() }
    }
}
