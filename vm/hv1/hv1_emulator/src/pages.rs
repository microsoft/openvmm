// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::VtlProtectAccess;
use guestmem::LockedPages;
use guestmem::Page;
use hvdef::HvMapGpaFlags;
use inspect::Inspect;
use safeatomic::AtomicSliceOps;
use std::ops::Deref;
use std::sync::atomic::AtomicU8;

pub(crate) struct LockedPage {
    page: LockedPages,
    pub gpn: u64,
}

impl LockedPage {
    pub fn new(gpn: u64, page: LockedPages) -> Self {
        assert!(page.pages().len() == 1);
        Self { page, gpn }
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
        new_gpn: u64,
        prot_access: &mut dyn VtlProtectAccess,
    ) -> Result<(), hvdef::HvError> {
        let new_page = prot_access.check_modify_and_lock_overlay_page(
            new_gpn,
            HvMapGpaFlags::new().with_readable(true).with_writable(true),
            None,
        )?;
        let new_page = LockedPage::new(new_gpn, new_page);
        new_page.atomic_write_obj(&self.atomic_read_obj::<[u8; 4096]>());

        self.unlock_prev_gpn(prot_access);

        *self = OverlayPage::Mapped(new_page);
        Ok(())
    }

    pub fn unmap(&mut self, prot_access: &mut dyn VtlProtectAccess) {
        let new_page = Box::new(std::array::from_fn(|_| AtomicU8::new(0)));
        new_page.atomic_write_obj(&self.atomic_read_obj::<[u8; 4096]>());

        self.unlock_prev_gpn(prot_access);

        *self = OverlayPage::Local(new_page);
    }

    fn unlock_prev_gpn(&mut self, prot_access: &mut dyn VtlProtectAccess) {
        if let Self::Mapped(page) = self {
            prot_access.unlock_overlay_page(page.gpn).unwrap();
        }
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
