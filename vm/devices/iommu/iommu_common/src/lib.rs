// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared IOMMU infrastructure for DMA translation and interrupt remapping.
//!
//! This crate provides:
//!
//! - A generic [`TranslatingMemory`] implementation of
//!   [`GuestMemoryAccess`](guestmem::GuestMemoryAccess) that translates IOVAs
//!   to GPAs via an [`IommuTranslator`] before delegating to an inner
//!   [`GuestMemory`].
//!
//! - An [`InterruptRemapper`] trait and [`RetranslateInterruptsList`] for
//!   routing pre-registered interrupt routes (IOAPIC, IrqFd) through an
//!   IOMMU's interrupt remapping table, with invalidation support.
//!
//! Both the ARM SMMUv3 and AMD IOMMU implementations use this crate to avoid
//! duplicating the per-page-boundary splitting, lock-across-translate-and-access
//! pattern, and `GuestMemoryAccess` boilerplate.

// UNSAFETY: needed to implement `GuestMemoryAccess`.
#![expect(unsafe_code)]

use guestmem::GuestMemory;
use guestmem::GuestMemoryBackingError;
use parking_lot::Mutex;
use pci_core::bus_range::AssignedBusRange;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::Weak;

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

/// Error returned when a requester ID's bus falls outside the device's
/// assigned bus range, faulting the DMA access.
#[derive(Debug, thiserror::Error)]
#[error(
    "DMA requester ID {rid:#06x} bus outside assigned bus range {secondary:#04x}..={subordinate:#04x}"
)]
struct RidOutOfRange {
    rid: u32,
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
    /// Offset added to `(secondary_bus << 8)` to form the requester ID for
    /// each access. For a single-function device this is just its devfn; for
    /// SR-IOV VFs it may carry into the bus byte to address functions on
    /// higher buses within the assigned range.
    rid_offset: u16,
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
        label: impl Into<Arc<str>>,
        translator: T,
        bus_range: AssignedBusRange,
        inner_gm: GuestMemory,
    ) -> GuestMemory {
        Self::new_guest_memory_for_rid_offset(label, translator, bus_range, 0, inner_gm)
    }

    /// Create a new translating `GuestMemory` for a requester-ID offset
    /// relative to the secondary bus.
    ///
    /// Every access resolves the RID as `(secondary_bus << 8) + rid_offset`,
    /// reading the secondary bus from the live `bus_range`, so the identity
    /// tracks the bus assignment. This is the primitive for SR-IOV virtual
    /// functions, which are constructed before the PF's bus is programmed.
    pub fn new_guest_memory_for_rid_offset(
        label: impl Into<Arc<str>>,
        translator: T,
        bus_range: AssignedBusRange,
        rid_offset: u16,
        inner_gm: GuestMemory,
    ) -> GuestMemory {
        let tm = TranslatingMemory {
            translator,
            bus_range,
            rid_offset,
            inner_gm,
        };
        GuestMemory::new(label, tm)
    }

    /// Derive the requester ID (RID) for a DMA access.
    ///
    /// The RID is composed as `(secondary_bus << 8) + rid_offset` against the
    /// live bus range. [`RidOutOfRange`] is returned when the resulting bus
    /// reaches past the subordinate bus — the access then faults rather than
    /// translating with a device identity outside the assigned range. The
    /// offset is non-negative, so the bus is always at least the secondary
    /// bus.
    fn rid(&self) -> Result<u16, RidOutOfRange> {
        let (secondary, subordinate) = self.bus_range.bus_range();
        let rid = ((secondary as u32) << 8) + self.rid_offset as u32;
        if rid >> 8 > subordinate as u32 {
            return Err(RidOutOfRange {
                rid,
                secondary,
                subordinate,
            });
        }
        Ok(rid as u16)
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
    label: Arc<str>,
    /// The IOMMU's arch-specific translator, cloned per derived VF.
    translator: T,
    /// The device's assigned bus range; the requester ID is derived as
    /// `(secondary << 8) + rid_offset` per access.
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
        label: impl Into<Arc<str>>,
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
    fn guest_memory_for_rid_offset(&self, rid_offset: u16) -> GuestMemory {
        TranslatingMemory::new_guest_memory_for_rid_offset(
            self.label.clone(),
            self.translator.clone(),
            self.bus_range.clone(),
            rid_offset,
            self.inner_gm.clone(),
        )
    }
}

/// Build a [`DmaTarget`](pci_core::dma::DmaTarget) backed by an IOMMU translator.
///
/// Builds a [`TranslatingDmaTarget`] factory over `translator`/`guest_memory`
/// and hands it to [`DmaTarget::with_iommu`](pci_core::dma::DmaTarget::with_iommu),
/// which derives the base function-`devfn` translating memory from it and
/// stamps the MSI identity from `msi`. IOMMU backends (SMMU, AMD-Vi, …) call
/// this to plug their arch-specific translator into the
/// [`DmaTarget`](pci_core::dma::DmaTarget) machinery.
pub fn new_dma_target<T>(
    label: &str,
    translator: T,
    bus_range: AssignedBusRange,
    devfn: u8,
    guest_memory: GuestMemory,
    msi: &pci_core::msi::MsiConnection,
) -> pci_core::dma::DmaTarget
where
    T: IommuTranslator + Clone,
{
    let iommu = Arc::new(TranslatingDmaTarget::new(
        format!("{label}-translating"),
        translator,
        bus_range.clone(),
        guest_memory,
    ));
    pci_core::dma::DmaTarget::with_iommu(bus_range, devfn, iommu, msi)
}

/// Build a hardware-nestable [`DmaTarget`](pci_core::dma::DmaTarget) backed by
/// an IOMMU translator.
///
/// Like [`new_dma_target`], but hands the resulting factory to
/// [`DmaTarget::with_nestable_iommu`](pci_core::dma::DmaTarget::with_nestable_iommu)
/// along with an opaque nesting `handle`, marking the device
/// [`DmaPassthrough::HardwareNestable`](pci_core::dma::DmaPassthrough::HardwareNestable).
/// Accel-capable IOMMUs (e.g. an SMMU programming the host IOMMU for nested
/// stage-1 translation) call this so the device may be VFIO-assigned; the
/// assignment backend downcasts `handle` to wire the device into the emulated
/// IOMMU.
pub fn new_nestable_dma_target<T>(
    label: &str,
    translator: T,
    bus_range: AssignedBusRange,
    devfn: u8,
    guest_memory: GuestMemory,
    handle: Arc<dyn std::any::Any + Send + Sync>,
    msi: &pci_core::msi::MsiConnection,
) -> pci_core::dma::DmaTarget
where
    T: IommuTranslator + Clone,
{
    let iommu = Arc::new(TranslatingDmaTarget::new(
        format!("{label}-translating"),
        translator,
        bus_range.clone(),
        guest_memory,
    ));
    pci_core::dma::DmaTarget::with_nestable_iommu(bus_range, devfn, iommu, handle, msi)
}

// =============================================================================
// Interrupt Remapping Infrastructure
// =============================================================================

/// Trait for IOMMU interrupt remapping backends.
///
/// Each IOMMU implementation (AMD IOMMU, SMMU, etc.) provides a type that
/// implements this trait. Consumers use it both for on-demand MSI translation
/// and for registering pre-cached routes that need retranslation on IOMMU
/// invalidation commands.
///
/// When the IOMMU is disabled or interrupt remapping is not enabled,
/// [`remap_msi`](InterruptRemapper::remap_msi) should return the input
/// values unchanged (passthrough).
///
/// On remapping faults, the implementation is responsible for queuing any
/// IOMMU-specific fault events internally before returning `None`.
pub trait InterruptRemapper: Send + Sync {
    /// Translate an MSI address/data pair for a device.
    ///
    /// Returns `Some((address, data))` with the translated values, or `None`
    /// if the MSI should be masked (remapping fault or abort).
    fn remap_msi(&self, device_id: u16, address: u64, data: u32) -> Option<(u64, u32)>;

    /// Register a route for invalidation callbacks.
    ///
    /// When the IOMMU processes an `INVALIDATE_INTERRUPT_TABLE` command
    /// matching this route's device ID, it will call
    /// [`RetranslateInterrupts::retranslate`] so the route can re-translate
    /// its cached MSI parameters.
    fn register_route(&self, route: &Arc<dyn RetranslateInterrupts>);

    /// Unregister a route. Dead (dropped) routes are also cleaned up
    /// lazily during invalidation.
    fn unregister_route(&self, route: &Arc<dyn RetranslateInterrupts>);
}

/// Trait for pre-registered interrupt routes that can be retranslated on
/// IOMMU invalidation.
///
/// Routes that cache translated MSI parameters (e.g., IOAPIC routes
/// registered with KVM, or IrqFd routes) implement this trait so the
/// IOMMU can ask them to retranslate when the guest issues an
/// `INVALIDATE_INTERRUPT_TABLE` command.
pub trait RetranslateInterrupts: Send + Sync {
    /// The device ID (BDF) this route is associated with.
    fn device_id(&self) -> u16;

    /// Re-translate all cached routes and re-push any that changed to
    /// the hypervisor.
    fn retranslate(&self);
}

/// Shared infrastructure for managing registered interrupt routes.
///
/// IOMMU implementations embed this struct and delegate
/// `register_route`/`unregister_route` to it. On invalidation commands,
/// call [`invalidate`](RetranslateInterruptsList::invalidate) to retranslate
/// matching routes.
///
/// Routes are stored as `Weak` references and cleaned up lazily.
pub struct RetranslateInterruptsList {
    routes: Mutex<Vec<Weak<dyn RetranslateInterrupts>>>,
}

impl RetranslateInterruptsList {
    /// Create an empty route list.
    pub fn new() -> Self {
        Self {
            routes: Mutex::new(Vec::new()),
        }
    }

    /// Register a route for invalidation callbacks.
    pub fn register(&self, route: &Arc<dyn RetranslateInterrupts>) {
        let mut routes = self.routes.lock();
        routes.push(Arc::downgrade(route));
    }

    /// Unregister a route. Removes all dead weak references as well.
    pub fn unregister(&self, route: &Arc<dyn RetranslateInterrupts>) {
        let mut routes = self.routes.lock();
        routes.retain(|w| w.upgrade().is_some_and(|r| !Arc::ptr_eq(&r, route)));
    }

    /// Invalidate routes for a specific device, or all routes if
    /// `device_id` is `None`.
    ///
    /// For each matching route, calls `retranslate`. Dead references are
    /// cleaned up lazily.
    pub fn invalidate(&self, device_id: Option<u16>) {
        let routes = {
            let mut routes = self.routes.lock();
            let mut live_routes = Vec::new();

            routes.retain(|weak| {
                let Some(route) = weak.upgrade() else {
                    return false; // dead, remove
                };
                live_routes.push(route);
                true
            });

            live_routes
        };

        for route in routes {
            if device_id.is_none() || device_id == Some(route.device_id()) {
                route.retranslate();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;

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

    struct ReentrantRoute {
        list: Arc<RetranslateInterruptsList>,
        retranslates: AtomicU32,
    }

    impl RetranslateInterrupts for ReentrantRoute {
        fn device_id(&self) -> u16 {
            assert!(self.list.routes.try_lock().is_some());
            7
        }

        fn retranslate(&self) {
            self.retranslates.fetch_add(1, Ordering::SeqCst);
            assert!(self.list.routes.try_lock().is_some());
        }
    }

    #[test]
    fn invalidate_does_not_hold_lock_during_retranslate() {
        let list = Arc::new(RetranslateInterruptsList::new());
        let route = Arc::new(ReentrantRoute {
            list: list.clone(),
            retranslates: AtomicU32::new(0),
        });
        let route_dyn = route.clone() as Arc<dyn RetranslateInterrupts>;
        list.register(&route_dyn);

        list.invalidate(Some(7));

        assert_eq!(route.retranslates.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn rid_offset_in_range_translates() {
        let inner = GuestMemory::allocate(0x1000);
        let gm = TranslatingMemory::new_guest_memory_for_rid_offset(
            "test",
            IdentityTranslator,
            bus_range(5, 10),
            (2 << 8) | 0x02, // secondary 5 + offset -> bus 7 within [5, 10]
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
    fn rid_offset_out_of_range_faults() {
        let inner = GuestMemory::allocate(0x1000);
        let mut buf = [0u8; 2];

        // secondary 5 + offset (6 << 8) -> bus 11, above subordinate 10 ->
        // access faults
        let gm_above = TranslatingMemory::new_guest_memory_for_rid_offset(
            "test",
            IdentityTranslator,
            bus_range(5, 10),
            6 << 8,
            inner.clone(),
        );
        assert!(gm_above.read_at(0x100, &mut buf).is_err());
        assert!(gm_above.write_at(0x100, &[1, 2]).is_err());
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
