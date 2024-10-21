// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to temporarily map a physical address to a virtual address, during
//! kernel start.

use super::addr_space;
use core::ops::Deref;
use core::ops::DerefMut;
use core::sync::atomic::compiler_fence;
use core::sync::atomic::Ordering::SeqCst;
use x86defs::Pte;

const PAGE_SIZE: usize = 0x1000;
const PAGE_MASK: usize = 0xfff;

pub struct Mapper {
    ptr: *mut (),
    pte: &'static mut Pte,
}

pub struct TemporaryMap<'a, T>(&'a mut T, &'a mut Mapper);

impl<T> Deref for TemporaryMap<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl<T> DerefMut for TemporaryMap<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
    }
}

impl Mapper {
    /// # Safety
    ///
    /// The caller must ensure there is only one `Mapper` for a given
    /// `index` at a time.
    pub unsafe fn new(index: usize) -> Self {
        // SAFETY: the caller guarantees that we have unique access.
        let pte = unsafe { &mut *addr_space::temp_ptes().add(index) };
        Self {
            pte,
            ptr: (addr_space::temporary_map() + index * PAGE_SIZE) as *mut (),
        }
    }

    /// Maps the given physical address into virtual address space for the
    /// lifetime of the return value.
    ///
    /// `pa` must be page aligned.
    ///
    /// # Safety
    /// The caller must ensure that the object being mapped is a valid `T`
    /// before they dereference it.
    #[track_caller]
    pub unsafe fn map<T>(&mut self, pa: u64) -> TemporaryMap<'_, T> {
        assert!((pa as usize & PAGE_MASK) + size_of::<T>() <= PAGE_SIZE);
        assert!(!self.pte.present());
        *self.pte = Pte::new()
            .with_address(pa & !(PAGE_MASK as u64))
            .with_present(true)
            .with_read_write(true);
        compiler_fence(SeqCst);
        // SAFETY: the caller guarantees that the physical address is valid.
        let ptr = unsafe { &mut *self.ptr.byte_add(pa as usize & PAGE_MASK).cast() };
        TemporaryMap(ptr, self)
    }
}

impl<T> Drop for TemporaryMap<'_, T> {
    fn drop(&mut self) {
        compiler_fence(SeqCst);
        *self.1.pte = Pte::new();
        // SAFETY: invalidating the previous mapping. This has no safety
        // requirements.
        unsafe {
            core::arch::asm! {
                "invlpg [{0}]",
                in(reg) self.1.ptr,
            }
        }
    }
}
