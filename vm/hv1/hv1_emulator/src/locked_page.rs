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
        let page = match guest_memory.lock_gpns(false, &[gpn]) {
            Ok(it) => it,
            Err(err) => {
                tracelimit::error_ratelimited!(
                    gpn,
                    err = &err as &dyn std::error::Error,
                    "Failed to lock page"
                );
                return Err(err);
            }
        };
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
