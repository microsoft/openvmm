// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use guestmem::GuestMemory;
use guestmem::LockedPages;
use guestmem::Page;
use std::ops::Deref;

pub(crate) struct LockedPage {
    page: LockedPages,
}

impl LockedPage {
    pub fn new(guest_memory: &GuestMemory, gpn: u64) -> Result<Self, guestmem::GuestMemoryError> {
        let page = guest_memory.lock_gpns(false, &[gpn])?;
        assert!(page.pages().len() == 1);
        Ok(Self { page })
    }
}

impl Deref for LockedPage {
    type Target = Page;

    fn deref(&self) -> &Self::Target {
        self.page.pages()[0]
    }
}
