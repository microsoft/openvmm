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
use virt::irqcon::IRQ_LINES;
use virt::irqcon::IoApicRouting;
use virt::irqcon::MsiRequest;

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
/// re-translated when the remapper connects and on each
/// [`retranslate`](RetranslateInterrupts::retranslate).
pub struct IoApicRoutingConnection {
    inner: Arc<IoApicRoutingInner>,
}

struct IoApicRoutingInner {
    /// The hypervisor's `IoApicRouting` implementation; final routes go here.
    hv_routing: Arc<dyn IoApicRouting>,
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
    /// redundant `set_irq_route` calls on retranslation.
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
        self.inner.set_all_routes(&mut state);
    }
}

impl IoApicRoutingInner {
    /// Translate the cached raw route for `irq` through the remapper (if any)
    /// and program it into `hv_routing`, skipping the call if unchanged.
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
    }

    /// Re-translate and re-program all cached routes.
    fn set_all_routes(&self, state: &mut IoApicRoutingState) {
        for irq in 0..IRQ_LINES {
            self.set_route(state, irq as u8);
        }
    }
}

impl IoApicRouting for IoApicRoutingInner {
    fn set_irq_route(&self, irq: u8, request: Option<MsiRequest>) {
        // Hold the lock across translate to serialize with retranslate().
        let mut state = self.state.lock();
        state.routes[irq as usize].raw = request;
        self.set_route(&mut state, irq);
    }

    fn assert_irq(&self, irq: u8) {
        // Route is already programmed in the hypervisor; just forward.
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
        let mut state = self.state.lock();
        self.set_all_routes(&mut state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        routes: iommu_common::RetranslateInterruptsList,
        seen_device_ids: Mutex<Vec<u16>>,
    }

    impl TestRemapper {
        fn new(data_delta: u32) -> Self {
            Self {
                data_delta: AtomicU32::new(data_delta),
                routes: iommu_common::RetranslateInterruptsList::new(),
                seen_device_ids: Mutex::new(Vec::new()),
            }
        }
    }

    impl InterruptRemapper for TestRemapper {
        fn remap_msi(&self, device_id: u16, address: u64, data: u32) -> Option<(u64, u32)> {
            self.seen_device_ids.lock().push(device_id);
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

        target.set_irq_route(4, Some(raw));
        assert_eq!(hv_routing.last_route(4), Some(raw));

        let remapper = Arc::new(TestRemapper::new(1));
        connection.connect_remapper(0x00A0, remapper.clone());
        assert_eq!(
            hv_routing.last_route(4),
            Some(MsiRequest {
                address: raw.address + 0x1000,
                data: raw.data + 1,
            })
        );

        remapper.data_delta.store(2, Ordering::SeqCst);
        remapper.routes.invalidate(None);
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
}
