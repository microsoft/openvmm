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

/// Error returned when a fixed requester ID override falls outside the
/// device's assigned bus range.
///
/// Produced by [`TranslatingMemory`] when a per-VF `rid_override`'s bus is not
/// within the assigned `(secondary, subordinate)` range, so the DMA access
/// faults instead of translating with an out-of-range device identity.
#[derive(Debug, thiserror::Error)]
#[error(
    "DMA requester ID {rid:#06x} bus outside assigned bus range {secondary:#04x}..={subordinate:#04x}"
)]
struct RidOutOfRange {
    rid: u16,
    secondary: u8,
    subordinate: u8,
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
    /// Optional fixed requester ID. When `Some`, this RID is used for every
    /// access instead of deriving it from `bus_range`. Used for SR-IOV
    /// virtual functions, which share the PF's bus range but need a
    /// per-function RID.
    rid_override: Option<u16>,
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
            rid_override: None,
            inner_gm,
        };
        GuestMemory::new(label, tm)
    }

    /// Create a new translating `GuestMemory` for a specific requester ID.
    ///
    /// Like [`new_guest_memory`](Self::new_guest_memory), but every access
    /// uses the given `rid` (`(bus << 8) | devfn`) instead of deriving it
    /// from `bus_range`. Used for SR-IOV virtual functions, which share the
    /// PF's bus range but each need a distinct RID.
    pub fn new_guest_memory_for_rid(
        label: impl Into<std::sync::Arc<str>>,
        translator: T,
        bus_range: AssignedBusRange,
        rid: u16,
        inner_gm: GuestMemory,
    ) -> GuestMemory {
        let tm = TranslatingMemory {
            translator,
            bus_range,
            rid_override: Some(rid),
            inner_gm,
        };
        GuestMemory::new(label, tm)
    }

    /// Derive the requester ID (RID) for a DMA access.
    ///
    /// When a fixed `rid_override` is set (SR-IOV VFs), its bus is validated
    /// against the assigned bus range and [`RidOutOfRange`] is returned if it
    /// falls outside — the access then faults rather than translating with a
    /// device identity outside the device's assigned range. Otherwise the RID
    /// is derived as `(secondary_bus as u16) << 8`; the secondary bus is
    /// always in range.
    fn rid(&self) -> Result<u16, RidOutOfRange> {
        if let Some(rid) = self.rid_override {
            let bus = (rid >> 8) as u8;
            if !self.bus_range.contains_bus(bus) {
                let (secondary, subordinate) = self.bus_range.bus_range();
                return Err(RidOutOfRange {
                    rid,
                    secondary,
                    subordinate,
                });
            }
            return Ok(rid);
        }
        let (secondary, _) = self.bus_range.bus_range();
        Ok((secondary as u16) << 8)
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

            let rid = self
                .rid()
                .map_err(|err| GuestMemoryBackingError::other(current_iova, err))?;
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

/// A [`DmaTargetIommu`](pci_core::dma::DmaTargetIommu) implementation that
/// produces per-RID translating [`GuestMemory`] from any [`IommuTranslator`].
///
/// IOMMU backends (SMMU, AMD-Vi, …) plug into the
/// [`DmaTarget`](pci_core::dma::DmaTarget) machinery by handing one of these
/// their arch-specific [`IommuTranslator`]. They do not need to depend on
/// `pci_core::dma` or construct [`GuestMemory`] for virtual functions
/// themselves — this type performs the RID composition and `GuestMemory`
/// construction generically.
pub struct TranslatingDmaTarget<T: IommuTranslator + Clone> {
    /// Debug label applied to each VF's `GuestMemory`.
    label: std::sync::Arc<str>,
    /// The IOMMU's arch-specific translator, cloned per derived VF.
    translator: T,
    /// The device's assigned bus range, used to derive the default
    /// function-0 RID in [`guest_memory_for_devfn`](Self::guest_memory_for_devfn).
    bus_range: AssignedBusRange,
    /// The raw (untranslated) guest memory.
    inner_gm: GuestMemory,
}

impl<T: IommuTranslator + Clone> TranslatingDmaTarget<T> {
    /// Creates a new per-RID `GuestMemory` factory.
    ///
    /// - `label`: debug label applied to each VF's `GuestMemory`
    /// - `translator`: the IOMMU's arch-specific translator (cloned per VF)
    /// - `bus_range`: the device's assigned bus range, used to derive the
    ///   default function-0 RID
    /// - `inner_gm`: the raw (untranslated) guest memory
    pub fn new(
        label: impl Into<std::sync::Arc<str>>,
        translator: T,
        bus_range: AssignedBusRange,
        inner_gm: GuestMemory,
    ) -> Self {
        Self {
            label: label.into(),
            translator,
            bus_range,
            inner_gm,
        }
    }
}

impl<T: IommuTranslator + Clone> pci_core::dma::DmaTargetIommu for TranslatingDmaTarget<T> {
    fn guest_memory_for_devfn(&self, devfn: u8) -> GuestMemory {
        // Compose the RID from the bus range's secondary bus + the given devfn.
        let (secondary, _) = self.bus_range.bus_range();
        let rid = (secondary as u16) << 8 | devfn as u16;
        self.guest_memory_for_rid(rid)
    }

    fn guest_memory_for_rid(&self, rid: u16) -> GuestMemory {
        TranslatingMemory::new_guest_memory_for_rid(
            self.label.clone(),
            self.translator.clone(),
            self.bus_range.clone(),
            rid,
            self.inner_gm.clone(),
        )
    }
}

/// Build a [`DmaTarget`](pci_core::dma::DmaTarget) backed by an IOMMU translator.
///
/// Wraps `guest_memory` with a function-0 translating [`GuestMemory`] (whose RID
/// is derived dynamically from `bus_range` on each access) and a
/// [`TranslatingDmaTarget`] factory for per-VF derivation, then bundles both
/// with `msi_target`. IOMMU backends (SMMU, AMD-Vi, …) call this to plug their
/// arch-specific translator into the [`DmaTarget`](pci_core::dma::DmaTarget)
/// machinery without re-implementing the two-part construction.
pub fn new_dma_target<T>(
    label: &str,
    translator: T,
    bus_range: AssignedBusRange,
    guest_memory: GuestMemory,
    msi_target: pci_core::msi::MsiTarget,
) -> pci_core::dma::DmaTarget
where
    T: IommuTranslator + Clone,
{
    let translating_gm = TranslatingMemory::new_guest_memory(
        format!("{label}-translating"),
        translator.clone(),
        bus_range.clone(),
        guest_memory.clone(),
    );
    let iommu = std::sync::Arc::new(TranslatingDmaTarget::new(
        format!("{label}-translating-vf"),
        translator,
        bus_range,
        guest_memory,
    ));
    pci_core::dma::DmaTarget::with_iommu(translating_gm, msi_target, iommu)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Identity translator: GPA == IOVA, never faults. Records nothing — the
    /// `rid` validation happens in `TranslatingMemory` before `translate` is
    /// ever called.
    #[derive(Clone)]
    struct IdentityTranslator;

    #[derive(Debug, thiserror::Error)]
    #[error("identity translator never faults")]
    struct NeverFault;

    impl IommuTranslator for IdentityTranslator {
        type Error = NeverFault;

        fn max_iova(&self) -> u64 {
            u64::MAX
        }

        fn translate<R>(
            &self,
            _rid: u16,
            iova: u64,
            _write: bool,
            op: impl FnOnce(u64) -> R,
        ) -> Result<R, TranslationFault<Self::Error>> {
            Ok(op(iova))
        }
    }

    fn bus_range(secondary: u8, subordinate: u8) -> AssignedBusRange {
        let r = AssignedBusRange::new();
        r.set_bus_range(secondary, subordinate);
        r
    }

    #[test]
    fn rid_override_in_range_translates() {
        let inner = GuestMemory::allocate(0x1000);
        let gm = TranslatingMemory::new_guest_memory_for_rid(
            "test",
            IdentityTranslator,
            bus_range(5, 10),
            (7 << 8) | 0x02, // bus 7 within [5, 10]
            inner.clone(),
        );

        gm.write_at(0x100, &[0xAB, 0xCD]).unwrap();
        let mut buf = [0u8; 2];
        gm.read_at(0x100, &mut buf).unwrap();
        assert_eq!(buf, [0xAB, 0xCD]);

        // Identity mapping wrote through to inner GPA 0x100.
        let mut inner_buf = [0u8; 2];
        inner.read_at(0x100, &mut inner_buf).unwrap();
        assert_eq!(inner_buf, [0xAB, 0xCD]);
    }

    #[test]
    fn rid_override_out_of_range_faults() {
        let inner = GuestMemory::allocate(0x1000);
        let mut buf = [0u8; 2];

        // bus 11, above subordinate 10 → access faults
        let gm_above = TranslatingMemory::new_guest_memory_for_rid(
            "test",
            IdentityTranslator,
            bus_range(5, 10),
            11 << 8,
            inner.clone(),
        );
        assert!(gm_above.read_at(0x100, &mut buf).is_err());
        assert!(gm_above.write_at(0x100, &[1, 2]).is_err());

        // bus 4, below secondary 5 → access faults
        let gm_below = TranslatingMemory::new_guest_memory_for_rid(
            "test",
            IdentityTranslator,
            bus_range(5, 10),
            4 << 8,
            inner.clone(),
        );
        assert!(gm_below.read_at(0x100, &mut buf).is_err());
    }

    #[test]
    fn derived_rid_uses_secondary_bus_in_range() {
        // No override: the derived RID uses the secondary bus, which is
        // always within the range, so the access translates.
        let inner = GuestMemory::allocate(0x1000);
        let gm = TranslatingMemory::new_guest_memory(
            "test",
            IdentityTranslator,
            bus_range(5, 10),
            inner.clone(),
        );

        gm.write_at(0x40, &[0x11]).unwrap();
        let mut buf = [0u8; 1];
        gm.read_at(0x40, &mut buf).unwrap();
        assert_eq!(buf, [0x11]);
    }
}
