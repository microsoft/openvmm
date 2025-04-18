// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use guestmem::GuestMemory;
use guestmem::LockedPages;
use guestmem::Page;
use inspect::Inspect;
use safeatomic::AtomicSliceOps;
use std::ops::Deref;
use std::sync::atomic::AtomicU8;

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

#[derive(Inspect)]
#[inspect(external_tag)]
pub(crate) enum OverlayPage {
    Local(#[inspect(skip)] Box<Page>),
    Mapped(#[inspect(skip)] LockedPage),
}

// FUTURE: Technically we should restore the prior contents of a mapped location when we
// remap/unmap it, but we don't know of any scenario that actually requires this.
impl OverlayPage {
    pub fn remap(
        &mut self,
        guest_memory: &GuestMemory,
        gpn: u64,
    ) -> Result<(), guestmem::GuestMemoryError> {
        let new_page = LockedPage::new(guest_memory, gpn)?;
        new_page.atomic_write_obj(&self.atomic_read_obj::<[u8; 4096]>());
        *self = OverlayPage::Mapped(new_page);
        Ok(())
    }

    pub fn unmap(&mut self) {
        let new_page = Box::new(std::array::from_fn(|_| AtomicU8::new(0)));
        new_page.atomic_write_obj(&self.atomic_read_obj::<[u8; 4096]>());
        *self = OverlayPage::Local(new_page);
    }
}

impl Deref for OverlayPage {
    type Target = Page;

    fn deref(&self) -> &Self::Target {
        match self {
            OverlayPage::Local(page) => page,
            OverlayPage::Mapped(page) => page,
        }
    }
}

impl Default for OverlayPage {
    fn default() -> Self {
        OverlayPage::Local(Box::new(std::array::from_fn(|_| AtomicU8::new(0))))
    }
}
