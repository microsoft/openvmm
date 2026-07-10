// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Overlay page management for synthetic registers backed by guest pages.

use crate::VtlProtectAccess;
use guestmem::LockedPages;
use guestmem::Page;
use hvdef::HvMapGpaFlags;
use inspect::Inspect;
use safeatomic::AtomicSliceOps;
use std::ops::Deref;
use std::sync::atomic::AtomicU8;

/// A single guest page locked into memory, tracked by its GPN.
pub struct LockedPage {
    page: LockedPages,
    /// The guest page number that backs this locked page.
    pub gpn: u64,
}

impl LockedPage {
    /// Creates a new locked page from a single-page [`LockedPages`] and its GPN.
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

/// A page that is either backed by a local (VMM-owned) buffer when no guest
/// page is mapped, or by a locked guest page when mapped. The logical page
/// contents follow the overlay: remapping or unmapping copies the current
/// contents to the new backing, matching hypervisor overlay semantics.
#[derive(Inspect)]
#[inspect(external_tag)]
pub enum OverlayPage {
    /// Backed by a VMM-owned buffer; no guest page is currently mapped.
    Local(#[inspect(skip)] Box<Page>),
    /// Backed by a locked guest page.
    Mapped(#[inspect(skip)] LockedPage),
}

// FUTURE: Technically we should restore the prior contents of a mapped location when we
// remap/unmap it, but we don't know of any scenario that actually requires this.
impl OverlayPage {
    /// Synchronizes the overlay's mapping to the desired state.
    ///
    /// When `enabled` is true, maps the overlay at `gpn`, remapping if the
    /// overlay is currently mapped at a different GPN and doing nothing if it
    /// is already mapped there. When `enabled` is false, unmaps the overlay if
    /// it is currently mapped.
    ///
    /// This encapsulates the common "remap on enable/move, unmap on disable"
    /// flow shared by the synthetic registers that are backed by an overlay
    /// page, using the overlay's own mapped GPN as the source of truth.
    pub fn sync(
        &mut self,
        enabled: bool,
        gpn: u64,
        prot_access: &mut dyn VtlProtectAccess,
    ) -> Result<(), hvdef::HvError> {
        let mapped_gpn = match self {
            OverlayPage::Mapped(page) => Some(page.gpn),
            OverlayPage::Local(_) => None,
        };
        if enabled {
            if mapped_gpn != Some(gpn) {
                self.remap(gpn, prot_access)?;
            }
        } else if mapped_gpn.is_some() {
            self.unmap(prot_access);
        }
        Ok(())
    }

    /// Maps the overlay to `new_gpn`, copying the current overlay contents into
    /// the newly mapped guest page and releasing any previously mapped page.
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

    /// Unmaps the overlay, copying the current contents back into a
    /// VMM-owned buffer and releasing the previously mapped guest page.
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

    /// Reads the full 4096-byte logical contents of the overlay page.
    pub fn save_page(&self) -> [u8; 4096] {
        self.atomic_read_obj()
    }

    /// Writes the full 4096-byte logical contents of the overlay page.
    pub fn restore_page(&self, data: &[u8; 4096]) {
        self.atomic_write_obj(data);
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
