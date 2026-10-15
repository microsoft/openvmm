// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(guest_arch = "x86_64")]

//! IOAPIC-to-IOMMU interrupt remapping wiring.
//!
//! Wraps an inner `virt::irqcon::IoApicRouting` to translate MSI
//! address/data through the IOMMU's interrupt remapping table before
//! pushing routes to the hypervisor.

use iommu_common::InterruptRemapper;
use iommu_common::RetranslateInterrupts;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use virt::irqcon::IRQ_LINES;
use virt::irqcon::IoApicRouting;
use virt::irqcon::MsiRequest;

/// Bitmask with a bit set for every valid IRQ line.
const ALL_IRQS: u32 = (1 << IRQ_LINES) - 1;

/// Device/function (devfn) used as the IOAPIC requestor ID (RID) for
/// interrupt remapping, as required by the Linux AMD-Vi driver.
///
/// Linux expects the southbridge IOAPIC RID to be 00:14.0
/// (devfn `0xA0` = device `0x14`, function 0) and disables
/// interrupt remapping entirely if a matching DEV_SPECIAL(IOAPIC) entry isn't
/// present in the IVRS. We reserve this devfn on segment 0 and publish it via
/// IVRS DEV_SPECIAL(IOAPIC) so Linux can resolve the IOAPIC RID for IRTE/DTE
/// lookup.
pub const IOAPIC_PHANTOM_DEVFN: u8 = 0xA0;

/// The x86 IOMMU selected to host the southbridge IOAPIC's interrupt
/// remapping context.
pub(super) struct IoapicIommuSelection {
    /// Interrupt remapper backing the IOAPIC routes. This is the same IOMMU
    /// whose ACPI table entry publishes [`ioapic_rid`](Self::ioapic_rid), so
    /// ACPI discovery and runtime remapping always agree.
    pub remapper: Arc<dyn InterruptRemapper>,
    /// IOAPIC PCIe Requester ID (RID) published in the IOMMU ACPI table.
    pub ioapic_rid: u16,
}

/// The control side of the IOAPIC interrupt-remapping connection.
///
/// Modeled after [`pci_core::msi::MsiConnection`](pci_core::msi::MsiConnection):
/// [`target`](Self::target) hands the routing interface to the IOAPIC device
/// (via the resolver) while it is still in passthrough mode, and
/// [`connect_remapper`](Self::connect_remapper) wires in the IOMMU later, once
/// it exists. The two roles share the same [`Inner`], but callers only see the
/// control handle and the `dyn IoApicRouting` target. Cached routes are
/// (re)translated lazily, at their next delivery, after the remapper connects
/// and on each [`retranslate`](RetranslateInterrupts::retranslate).
pub struct IoApicRoutingConnection {
    inner: Arc<IoApicRoutingInner>,
}

struct IoApicRoutingInner {
    /// The hypervisor's `IoApicRouting` implementation; final routes go here.
    hv_routing: Arc<dyn IoApicRouting>,
    /// Bitmask (one bit per IRQ) of routes whose cached translation is stale
    /// and must be re-translated before their next delivery.
    ///
    /// Kept outside [`state`](Self::state) so the hot [`assert_irq`] path can
    /// test it with a single relaxed load and forward the interrupt without
    /// taking the lock; only a route that was (re)programmed or invalidated
    /// takes the lock to translate. Every write happens under `state`, so
    /// writers never race each other and use plain relaxed load/stores (see
    /// [`mark_dirty`]/[`clear_dirty`]) rather than atomic read-modify-writes;
    /// the lock-free reader in `assert_irq` is the only unsynchronized accessor.
    ///
    /// [`assert_irq`]: IoApicRoutingInner::assert_irq
    /// [`mark_dirty`]: IoApicRoutingInner::mark_dirty
    /// [`clear_dirty`]: IoApicRoutingInner::clear_dirty
    dirty: AtomicU32,
    state: Mutex<IoApicRoutingState>,
}

struct IoApicRoutingState {
    /// Remapping state; `None` while in passthrough mode.
    remap: Option<RemapState>,
    /// Per-IRQ route state.
    routes: [Route; IRQ_LINES],
}

#[derive(Copy, Clone, Default)]
struct Route {
    /// Raw (pre-remapping) MSI request from the IOAPIC.
    raw: Option<MsiRequest>,
    /// Last translated route programmed into `hv_routing`. Used to skip
    /// redundant `set_irq_route` calls.
    programmed: Option<MsiRequest>,
}

struct RemapState {
    /// IOAPIC RID (used for DTE/IRTE lookup).
    rid: u16,
    /// The IOMMU's interrupt remapper.
    remapper: Arc<dyn InterruptRemapper>,
}

impl IoApicRoutingConnection {
    /// Create a connection in passthrough mode, forwarding routes to
    /// `hv_routing`.
    pub fn new(hv_routing: Arc<dyn IoApicRouting>) -> Self {
        Self {
            inner: Arc::new(IoApicRoutingInner {
                hv_routing,
                dirty: AtomicU32::new(0),
                state: Mutex::new(IoApicRoutingState {
                    remap: None,
                    routes: [Route::default(); IRQ_LINES],
                }),
            }),
        }
    }

    /// The routing interface handed to the IOAPIC device via the resolver.
    pub fn target(&self) -> Arc<dyn IoApicRouting> {
        self.inner.clone()
    }

    /// Transition into remapping mode, re-translating already-programmed routes.
    ///
    /// Panics if called more than once.
    pub fn connect_remapper(&self, rid: u16, remapper: Arc<dyn InterruptRemapper>) {
        // Register before recording the remapper so an invalidation racing
        // this can't be missed (it retranslates as a harmless passthrough
        // until `remap` is set). Register outside the state lock: `invalidate`
        // takes the route-list lock then the state lock, so the reverse order
        // here would deadlock.
        remapper.register_route(&(self.inner.clone() as Arc<dyn RetranslateInterrupts>));
        let mut state = self.inner.state.lock();
        assert!(
            state.remap.is_none(),
            "IOAPIC remapper connected more than once"
        );
        state.remap = Some(RemapState { rid, remapper });
        // Routes are (re)translated lazily; mark them all dirty so each is
        // translated on its next delivery rather than translating eagerly here.
        self.inner.mark_dirty(ALL_IRQS);
    }
}

impl IoApicRoutingInner {
    /// Translate the cached raw route for `irq` through the remapper (if any)
    /// and program it into `hv_routing`, skipping the call if unchanged.
    ///
    /// This is the delivery-time translation: the remapper's IRTE lookup
    /// (which may record a fault) runs here, so callers must only invoke it
    /// when the interrupt is actually being delivered. Must be called with
    /// `state` held; clears the route's dirty bit.
    fn set_route(&self, state: &mut IoApicRoutingState, irq: u8) {
        let route = &mut state.routes[irq as usize];
        let translated = match &state.remap {
            Some(remap) => route.raw.and_then(|r| {
                remap
                    .remapper
                    .remap_msi(remap.rid, r.address, r.data)
                    .map(|(address, data)| MsiRequest { address, data })
            }),
            None => route.raw,
        };
        if route.programmed != translated {
            route.programmed = translated;
            self.hv_routing.set_irq_route(irq, translated);
        }
        self.clear_dirty(1 << irq);
    }

    /// Mark the IRQ lines in `mask` dirty. Callers must hold `state`: since
    /// every writer is serialized by that lock there is no racing writer, so a
    /// plain load/store suffices rather than an atomic read-modify-write. The
    /// only lock-free accessor is the reader in `assert_irq`.
    fn mark_dirty(&self, mask: u32) {
        self.dirty
            .store(self.dirty.load(Ordering::Relaxed) | mask, Ordering::Relaxed);
    }

    /// Clear the dirty bits for the IRQ lines in `mask`. Callers must hold
    /// `state`.
    fn clear_dirty(&self, mask: u32) {
        self.dirty.store(
            self.dirty.load(Ordering::Relaxed) & !mask,
            Ordering::Relaxed,
        );
    }
}

impl IoApicRouting for IoApicRoutingInner {
    fn set_irq_route(&self, irq: u8, request: Option<MsiRequest>) {
        // Hold the lock across the update to serialize with retranslate().
        let mut state = self.state.lock();
        state.routes[irq as usize].raw = request;
        if state.remap.is_some() {
            // Remapping mode: defer translation until the interrupt is actually
            // delivered to avoid spurious faults for routes that never fire.
            self.mark_dirty(1 << irq);
        } else {
            // Passthrough mode: no IOMMU, so program the route directly.
            self.set_route(&mut state, irq);
        }
    }

    fn assert_irq(&self, irq: u8) {
        if self.dirty.load(Ordering::Relaxed) & (1 << irq) != 0 {
            let mut state = self.state.lock();
            // Re-check under the lock: another thread may have translated it.
            if self.dirty.load(Ordering::Relaxed) & (1 << irq) != 0 {
                self.set_route(&mut state, irq);
            }
        }
        self.hv_routing.assert_irq(irq);
    }
}

impl RetranslateInterrupts for IoApicRoutingInner {
    fn device_id(&self) -> u16 {
        self.state
            .lock()
            .remap
            .as_ref()
            .map_or(0, |remap| remap.rid)
    }

    fn retranslate(&self) {
        // Mark every route for re-translation at its next delivery rather than
        // translating now: translating (and potentially faulting) a route that
        // never fires would record a spurious fault. Take the state lock so
        // this cannot interleave with a `set_route` clearing a dirty bit for a
        // stale (pre-invalidation) translation. See [`IoApicRoutingInner::dirty`].
        let _state = self.state.lock();
        self.mark_dirty(ALL_IRQS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;

    #[derive(Default)]
    struct RecordingIoApicRouting {
        routes: Mutex<Vec<(u8, Option<MsiRequest>)>>,
    }

    impl RecordingIoApicRouting {
        fn last_route(&self, irq: u8) -> Option<MsiRequest> {
            self.routes
                .lock()
                .iter()
                .rev()
                .find(|(route_irq, _)| *route_irq == irq)
                .and_then(|(_, request)| *request)
        }
    }

    impl IoApicRouting for RecordingIoApicRouting {
        fn set_irq_route(&self, irq: u8, request: Option<MsiRequest>) {
            self.routes.lock().push((irq, request));
        }

        fn assert_irq(&self, _irq: u8) {}
    }

    struct TestRemapper {
        data_delta: AtomicU32,
        /// When set, `remap_msi` returns `None` (an interrupt-remapping fault).
        fault: AtomicBool,
        routes: iommu_common::RetranslateInterruptsList,
        seen_device_ids: Mutex<Vec<u16>>,
    }

    impl TestRemapper {
        fn new(data_delta: u32) -> Self {
            Self {
                data_delta: AtomicU32::new(data_delta),
                fault: AtomicBool::new(false),
                routes: iommu_common::RetranslateInterruptsList::new(),
                seen_device_ids: Mutex::new(Vec::new()),
            }
        }
    }

    impl InterruptRemapper for TestRemapper {
        fn remap_msi(&self, device_id: u16, address: u64, data: u32) -> Option<(u64, u32)> {
            self.seen_device_ids.lock().push(device_id);
            if self.fault.load(Ordering::SeqCst) {
                return None;
            }
            Some((
                address + 0x1000,
                data + self.data_delta.load(Ordering::SeqCst),
            ))
        }

        fn register_route(&self, route: &Arc<dyn RetranslateInterrupts>) {
            self.routes.register(route);
        }

        fn unregister_route(&self, route: &Arc<dyn RetranslateInterrupts>) {
            self.routes.unregister(route);
        }
    }

    #[test]
    fn ioapic_routes_are_remapped_and_retranslated() {
        let hv_routing = Arc::new(RecordingIoApicRouting::default());
        let connection = IoApicRoutingConnection::new(hv_routing.clone());
        let target = connection.target();
        let raw = MsiRequest {
            address: 0xFEE0_0000,
            data: 0x20,
        };

        // Passthrough mode: the route is programmed directly.
        target.set_irq_route(4, Some(raw));
        assert_eq!(hv_routing.last_route(4), Some(raw));

        // Connecting the remapper defers translation to the next delivery.
        let remapper = Arc::new(TestRemapper::new(1));
        connection.connect_remapper(0x00A0, remapper.clone());
        assert_eq!(hv_routing.last_route(4), Some(raw));

        // Delivery triggers translation.
        target.assert_irq(4);
        assert_eq!(
            hv_routing.last_route(4),
            Some(MsiRequest {
                address: raw.address + 0x1000,
                data: raw.data + 1,
            })
        );

        // Invalidation defers re-translation to the next delivery.
        remapper.data_delta.store(2, Ordering::SeqCst);
        remapper.routes.invalidate(None);
        assert_eq!(
            hv_routing.last_route(4),
            Some(MsiRequest {
                address: raw.address + 0x1000,
                data: raw.data + 1,
            })
        );

        // Delivery re-translates against the updated table.
        target.assert_irq(4);
        assert_eq!(
            hv_routing.last_route(4),
            Some(MsiRequest {
                address: raw.address + 0x1000,
                data: raw.data + 2,
            })
        );
        assert!(
            remapper
                .seen_device_ids
                .lock()
                .iter()
                .all(|device_id| *device_id == 0x00A0)
        );
    }

    #[test]
    fn translation_is_deferred_until_delivery() {
        let hv_routing = Arc::new(RecordingIoApicRouting::default());
        let connection = IoApicRoutingConnection::new(hv_routing.clone());
        let target = connection.target();
        let remapper = Arc::new(TestRemapper::new(1));
        connection.connect_remapper(0x00A0, remapper.clone());

        // Programming a route must not translate it: no interrupt has fired.
        target.set_irq_route(
            4,
            Some(MsiRequest {
                address: 0xFEE0_0000,
                data: 0x20,
            }),
        );
        assert!(remapper.seen_device_ids.lock().is_empty());

        // Neither must an invalidation.
        remapper.routes.invalidate(None);
        assert!(remapper.seen_device_ids.lock().is_empty());

        // Only actual delivery triggers the IRTE lookup.
        target.assert_irq(4);
        assert_eq!(remapper.seen_device_ids.lock().len(), 1);
    }

    #[test]
    fn faulting_route_is_looked_up_and_masked_only_on_delivery() {
        let hv_routing = Arc::new(RecordingIoApicRouting::default());
        let connection = IoApicRoutingConnection::new(hv_routing.clone());
        let target = connection.target();
        let remapper = Arc::new(TestRemapper::new(1));
        remapper.fault.store(true, Ordering::SeqCst);
        connection.connect_remapper(0x00A0, remapper.clone());

        // Program an unmasked route whose lookup will fault. Because it is
        // never delivered, the remapper must not be consulted — this is what
        // keeps a never-firing entry from recording a spurious remapping fault.
        target.set_irq_route(
            4,
            Some(MsiRequest {
                address: 0xFEE0_0000,
                data: 0x20,
            }),
        );
        assert!(remapper.seen_device_ids.lock().is_empty());

        // On delivery the lookup runs and, faulting, masks the hv route.
        target.assert_irq(4);
        assert_eq!(remapper.seen_device_ids.lock().len(), 1);
        assert_eq!(hv_routing.last_route(4), None);
    }

    #[test]
    fn masked_route_is_not_looked_up() {
        let hv_routing = Arc::new(RecordingIoApicRouting::default());
        let connection = IoApicRoutingConnection::new(hv_routing.clone());
        let target = connection.target();
        let remapper = Arc::new(TestRemapper::new(1));
        connection.connect_remapper(0x00A0, remapper.clone());

        // A masked (None) route must never invoke the remapper, even on
        // delivery.
        target.set_irq_route(4, None);
        target.assert_irq(4);
        assert!(remapper.seen_device_ids.lock().is_empty());
    }

    #[test]
    fn clean_route_is_not_retranslated_without_invalidation() {
        let hv_routing = Arc::new(RecordingIoApicRouting::default());
        let connection = IoApicRoutingConnection::new(hv_routing.clone());
        let target = connection.target();
        let remapper = Arc::new(TestRemapper::new(1));
        connection.connect_remapper(0x00A0, remapper.clone());
        target.set_irq_route(
            4,
            Some(MsiRequest {
                address: 0xFEE0_0000,
                data: 0x20,
            }),
        );

        // First delivery translates once.
        target.assert_irq(4);
        assert_eq!(remapper.seen_device_ids.lock().len(), 1);

        // Further deliveries without an invalidation reuse the cached route.
        target.assert_irq(4);
        target.assert_irq(4);
        assert_eq!(remapper.seen_device_ids.lock().len(), 1);

        // An invalidation forces exactly one re-translation on the next
        // delivery, not on every delivery after it.
        remapper.routes.invalidate(None);
        target.assert_irq(4);
        target.assert_irq(4);
        assert_eq!(remapper.seen_device_ids.lock().len(), 2);
    }

    #[test]
    fn dirty_state_is_tracked_per_irq() {
        let hv_routing = Arc::new(RecordingIoApicRouting::default());
        let connection = IoApicRoutingConnection::new(hv_routing.clone());
        let target = connection.target();
        let remapper = Arc::new(TestRemapper::new(1));
        connection.connect_remapper(0x00A0, remapper.clone());

        let raw4 = MsiRequest {
            address: 0xFEE0_0000,
            data: 0x20,
        };
        let raw7 = MsiRequest {
            address: 0xFEE0_0000,
            data: 0x21,
        };
        target.set_irq_route(4, Some(raw4));
        target.set_irq_route(7, Some(raw7));

        // Delivering IRQ 4 translates only IRQ 4; IRQ 7 stays deferred.
        target.assert_irq(4);
        assert_eq!(
            hv_routing.last_route(4),
            Some(MsiRequest {
                address: raw4.address + 0x1000,
                data: raw4.data + 1,
            })
        );
        assert_eq!(hv_routing.last_route(7), None);
        assert_eq!(remapper.seen_device_ids.lock().len(), 1);

        // Delivering IRQ 7 translates it independently.
        target.assert_irq(7);
        assert_eq!(
            hv_routing.last_route(7),
            Some(MsiRequest {
                address: raw7.address + 0x1000,
                data: raw7.data + 1,
            })
        );
        assert_eq!(remapper.seen_device_ids.lock().len(), 2);
    }
}
