// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared IOMMU DMA translation infrastructure.
//!
//! This crate provides a generic [`TranslatingMemory`] implementation of
//! [`GuestMemoryAccess`](guestmem::GuestMemoryAccess) that translates IOVAs
//! to GPAs via an [`IommuTranslator`] before delegating to an inner
//! [`GuestMemory`].
//!
//! Both the ARM SMMUv3 and AMD IOMMU implementations use this crate to avoid
//! duplicating the per-page-boundary splitting, lock-across-translate-and-access
//! pattern, and `GuestMemoryAccess` boilerplate.

// UNSAFETY: needed to implement `GuestMemoryAccess`.
#![expect(unsafe_code)]

use guestmem::GuestMemory;
use guestmem::GuestMemoryBackingError;
use pci_core::bus_range::AssignedBusRange;
use std::ptr::NonNull;

/// Trait for IOMMU translation backends.
///
/// Each IOMMU implementation (SMMU, AMD IOMMU, etc.) provides a type that
/// implements this trait. The [`translate`](IommuTranslator::translate) method
/// acquires whatever lock the IOMMU needs, translates the IOVA to a GPA,
/// calls the provided closure while the lock is held, and returns the result.
///
/// The closure-based API preserves the TOCTOU invariant: the GPA cannot go
/// stale between translation and use because the IOMMU's lock is held across
/// both operations.
///
/// The `rid` (requester ID / BDF) parameter identifies the device making the
/// DMA request. The translator maps it to the IOMMU-specific device identity
/// (stream ID for SMMU, DeviceID for AMD IOMMU) and uses it for page-table
/// lookup.
pub trait IommuTranslator: Send + Sync + 'static {
    /// The IOMMU-specific error type for translation faults.
    type Error: std::error::Error + Send + Sync + 'static;

    /// The exclusive upper bound of translatable IOVAs.
    ///
    /// This is typically `1 << va_bits` for the IOMMU's virtual address
    /// width. Used as the `max_address` for the `GuestMemoryAccess`
    /// implementation, which rejects out-of-range accesses before they
    /// reach the translator.
    fn max_iova(&self) -> u64;

    /// Translate an IOVA and execute `op` with the resulting GPA while the
    /// IOMMU's translation lock is held.
    ///
    /// - `rid`: requester ID (BDF) of the device making the DMA request
    /// - `iova`: the I/O virtual address to translate
    /// - `write`: whether this is a write access
    /// - `op`: closure called with the translated GPA; its return value is
    ///   forwarded to the caller
    ///
    /// On translation failure, the implementation should queue any
    /// IOMMU-specific fault events internally before returning `Err`.
    fn translate<R>(
        &self,
        rid: u16,
        iova: u64,
        write: bool,
        op: impl FnOnce(u64) -> R,
    ) -> Result<R, TranslationFault<Self::Error>>;
}

/// A translation fault returned by [`IommuTranslator::translate`].
///
/// The IOMMU-specific event/fault has already been queued by the translator;
/// this error carries enough information for the `GuestMemoryAccess` layer
/// to produce a [`GuestMemoryBackingError`].
#[derive(Debug, thiserror::Error)]
#[error("IOMMU translation fault at IOVA {iova:#x}")]
pub struct TranslationFault<E: std::error::Error + 'static> {
    /// The faulting IOVA.
    pub iova: u64,
    /// The IOMMU-specific error.
    #[source]
    pub error: E,
}

/// A [`GuestMemoryAccess`](guestmem::GuestMemoryAccess) implementation that
/// translates IOVAs via an [`IommuTranslator`] before accessing guest memory.
///
/// Each PCI device behind an IOMMU gets its own `TranslatingMemory`. DMA
/// accesses are split at 4KB page boundaries (since each page may have a
/// different translation), and the IOMMU's lock is held across translation
/// and memory access for each chunk.
pub struct TranslatingMemory<T: IommuTranslator> {
    /// The IOMMU-specific translator.
    translator: T,
    /// The device's assigned bus range, used to derive the RID.
    bus_range: AssignedBusRange,
    /// The raw (untranslated) guest memory.
    inner_gm: GuestMemory,
}

impl<T: IommuTranslator> TranslatingMemory<T> {
    /// Create a new `GuestMemory` that translates IOVAs via the given translator.
    ///
    /// The `bus_range` is used to derive the requester ID (RID) at each DMA
    /// access: `(secondary_bus << 8)`. If the secondary bus is 0, the RID is
    /// 0 and the IOMMU translates or faults accordingly.
    pub fn new_guest_memory(
        label: impl Into<std::sync::Arc<str>>,
        translator: T,
        bus_range: AssignedBusRange,
        inner_gm: GuestMemory,
    ) -> GuestMemory {
        let tm = TranslatingMemory {
            translator,
            bus_range,
            inner_gm,
        };
        GuestMemory::new(label, tm)
    }

    /// Derive the requester ID (RID) from the current bus range.
    ///
    /// Returns `(secondary_bus as u16) << 8`. If secondary_bus is 0,
    /// the RID is 0 — the IOMMU handles this case (translation or fault).
    fn rid(&self) -> u16 {
        let (secondary, _) = self.bus_range.bus_range();
        (secondary as u16) << 8
    }
}

/// Compute the size of the next chunk for a page-splitting DMA access.
///
/// Returns the number of bytes from `iova` to the end of the current 4KB
/// page, or `remaining` if that is smaller.
fn chunk_size(iova: u64, remaining: usize) -> usize {
    let page_offset = (iova & 0xFFF) as usize;
    let bytes_in_page = 0x1000 - page_offset;
    remaining.min(bytes_in_page)
}

impl<T: IommuTranslator> TranslatingMemory<T> {
    /// Perform a translated memory operation, splitting at page boundaries.
    ///
    /// For each 4KB-aligned chunk, calls `translator.translate()` which holds
    /// the IOMMU lock across both translation and the memory access closure.
    fn do_translated_op(
        &self,
        iova: u64,
        len: usize,
        write: bool,
        mut op: impl FnMut(u64, usize, usize) -> Result<(), GuestMemoryBackingError>,
    ) -> Result<(), GuestMemoryBackingError> {
        let mut offset = 0usize;
        while offset < len {
            let current_iova = iova + offset as u64;
            let chunk_len = chunk_size(current_iova, len - offset);

            let rid = self.rid();
            let result = self
                .translator
                .translate(rid, current_iova, write, |gpa| op(gpa, offset, chunk_len));

            match result {
                Ok(inner_result) => inner_result?,
                Err(fault) => {
                    return Err(GuestMemoryBackingError::other(current_iova, fault));
                }
            }

            offset += chunk_len;
        }

        Ok(())
    }
}

// SAFETY: TranslatingMemory returns `None` from `mapping()`, so the caller
// never gets a raw pointer. All accesses go through the fallback methods which
// translate IOVAs to GPAs and delegate to the inner GuestMemory.
unsafe impl<T: IommuTranslator> guestmem::GuestMemoryAccess for TranslatingMemory<T> {
    fn mapping(&self) -> Option<NonNull<u8>> {
        None
    }

    fn max_address(&self) -> u64 {
        self.translator.max_iova()
    }

    unsafe fn read_fallback(
        &self,
        addr: u64,
        dest: *mut u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        self.do_translated_op(addr, len, false, |gpa, offset, chunk_len| {
            // SAFETY: dest is valid for len bytes per the trait contract.
            let chunk_dest = unsafe { std::slice::from_raw_parts_mut(dest.add(offset), chunk_len) };
            self.inner_gm
                .read_at(gpa, chunk_dest)
                .map_err(|e| GuestMemoryBackingError::other(addr, e))
        })
    }

    unsafe fn write_fallback(
        &self,
        addr: u64,
        src: *const u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        self.do_translated_op(addr, len, true, |gpa, offset, chunk_len| {
            // SAFETY: src is valid for len bytes per the trait contract.
            let chunk_src = unsafe { std::slice::from_raw_parts(src.add(offset), chunk_len) };
            self.inner_gm
                .write_at(gpa, chunk_src)
                .map_err(|e| GuestMemoryBackingError::other(addr, e))
        })
    }

    fn fill_fallback(&self, addr: u64, val: u8, len: usize) -> Result<(), GuestMemoryBackingError> {
        self.do_translated_op(addr, len, true, |gpa, _offset, chunk_len| {
            self.inner_gm
                .fill_at(gpa, val, chunk_len)
                .map_err(|e| GuestMemoryBackingError::other(addr, e))
        })
    }
}
