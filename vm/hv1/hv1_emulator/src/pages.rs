// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: Implementing a helper type for accessing locked overlay pages via
// raw pointers.
#![expect(unsafe_code)]

use crate::VtlProtectAccess;
use hvdef::HvMapGpaFlags;
use inspect::Inspect;
use safeatomic::AtomicSliceOps;
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::atomic::AtomicU8;

const PAGE_LEN: usize = 4096;
type Page = [AtomicU8; PAGE_LEN];

/// A locked overlay page wrapper that is used to access the page's contents safely.
pub struct LockedOverlayPage {
    page: NonNull<Page>,
    pub(crate) gpn: u64,
}

// SAFETY: The raw pointer is just a pointer with no methods and has no inherent safety
// constraints.
unsafe impl Send for LockedOverlayPage {}

// SAFETY: The raw pointer is just a pointer with no methods and has no inherent safety
// constraints.
unsafe impl Sync for LockedOverlayPage {}

impl LockedOverlayPage {
    /// Creates a new `LockedOverlayPage` from a raw pointer to a page and its guest page number (GPN).
    ///
    /// # Safety
    /// The caller must ensure that the `page` pointer is valid and points to a page
    /// that is locked and accessible as long as this struct exists. The `gpn` must
    /// also be valid and correspond to the page being pointed to.
    pub unsafe fn new(page: NonNull<Page>, gpn: u64) -> Self {
        Self { page, gpn }
    }
}

impl Deref for LockedOverlayPage {
    type Target = Page;

    fn deref(&self) -> &Self::Target {
        // SAFETY: The pointer is guaranteed to be valid as long as this type
        // exists by the caller of `new`.
        unsafe { self.page.as_ref() }
    }
}

#[derive(Inspect)]
#[inspect(external_tag)]
pub(crate) enum OverlayPage {
    Local(#[inspect(skip)] Box<Page>),
    Mapped(#[inspect(skip)] LockedOverlayPage),
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
        new_page.atomic_write_obj(&self.atomic_read_obj::<[u8; PAGE_LEN]>());

        self.unlock_prev_gpn(prot_access);

        *self = OverlayPage::Mapped(new_page);
        Ok(())
    }

    pub fn unmap(&mut self, prot_access: &mut dyn VtlProtectAccess) {
        let new_page = Box::new(std::array::from_fn(|_| AtomicU8::new(0)));
        new_page.atomic_write_obj(&self.atomic_read_obj::<[u8; PAGE_LEN]>());

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
