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
use virt::irqcon::IoApicRouting;
use virt::irqcon::MsiRequest;

/// Number of IOAPIC redirection table entries.
const NUM_ENTRIES: usize = 24;

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
    /// Raw (pre-remapping) MSI requests from the IOAPIC, indexed by IRQ.
    raw_routes: [Option<MsiRequest>; NUM_ENTRIES],
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
                    raw_routes: [None; NUM_ENTRIES],
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
        {
            let mut state = self.inner.state.lock();
            assert!(
                state.remap.is_none(),
                "IOAPIC remapper connected more than once"
            );
            state.remap = Some(RemapState {
                rid,
                remapper: remapper.clone(),
            });
            self.inner.set_all_routes(&state);
        }
        // Register outside the lock: `register_route` may synchronously invoke
        // `retranslate`, which acquires the same lock.
        remapper.register_route(&(self.inner.clone() as Arc<dyn RetranslateInterrupts>));
    }
}

impl IoApicRoutingInner {
    /// Translate `raw` through the remapper (if any) and program it into
    /// `hv_routing`.
    fn set_route(&self, state: &IoApicRoutingState, irq: u8, raw: Option<MsiRequest>) {
        let translated = match &state.remap {
            Some(remap) => raw.and_then(|r| {
                remap
                    .remapper
                    .remap_msi(remap.rid, r.address, r.data)
                    .map(|(address, data)| MsiRequest { address, data })
            }),
            None => raw,
        };
        self.hv_routing.set_irq_route(irq, translated);
    }

    /// Re-translate and re-program all cached routes.
    fn set_all_routes(&self, state: &IoApicRoutingState) {
        for (irq, raw) in state.raw_routes.iter().enumerate() {
            self.set_route(state, irq as u8, *raw);
        }
    }
}

impl IoApicRouting for IoApicRoutingInner {
    fn set_irq_route(&self, irq: u8, request: Option<MsiRequest>) {
        // Hold the lock across translate to serialize with retranslate().
        let mut state = self.state.lock();
        if let Some(slot) = state.raw_routes.get_mut(irq as usize) {
            *slot = request;
        }
        self.set_route(&state, irq, request);
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
        let state = self.state.lock();
        self.set_all_routes(&state);
    }
}
