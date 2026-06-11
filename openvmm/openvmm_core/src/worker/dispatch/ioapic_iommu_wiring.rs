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
use parking_lot::RwLock;
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

/// An `IoApicRouting` implementation that delegates to a swappable inner.
///
/// Registered early so that the base chipset build can resolve the IOAPIC
/// routing resource. After IOMMU setup, the inner is swapped to an
/// [`IommuIoApicRouting`] wrapper that remaps through the IOMMU.
pub struct SwappableIoApicRouting {
    inner: RwLock<Arc<dyn IoApicRouting>>,
}

impl SwappableIoApicRouting {
    /// Create with an initial (plain) routing implementation.
    pub fn new(inner: Arc<dyn IoApicRouting>) -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(inner),
        })
    }

    /// Replace the inner routing implementation.
    pub fn swap(&self, new_inner: Arc<dyn IoApicRouting>) {
        *self.inner.write() = new_inner;
    }
}

impl IoApicRouting for SwappableIoApicRouting {
    fn set_irq_route(&self, irq: u8, request: Option<MsiRequest>) {
        self.inner.read().set_irq_route(irq, request);
    }

    fn assert_irq(&self, irq: u8) {
        self.inner.read().assert_irq(irq);
    }
}

/// An `IoApicRouting` wrapper that remaps MSI address/data through an
/// IOMMU's interrupt remapping table.
///
/// Stores the raw (guest-written) MSI parameters for each IRQ line and
/// translates them through the IOMMU before forwarding to the inner
/// `IoApicRouting` implementation.
///
/// On `INVALIDATE_INTERRUPT_TABLE`, the IOMMU calls [`retranslate`],
/// which re-translates all stored routes and re-pushes any that changed.
pub struct IommuIoApicRouting {
    inner: Arc<dyn IoApicRouting>,
    rid: u16,
    remapper: Arc<dyn InterruptRemapper>,
    /// Raw (pre-remapping) MSI requests from the IOAPIC, indexed by IRQ.
    raw_routes: Mutex<[Option<MsiRequest>; NUM_ENTRIES]>,
}

impl IommuIoApicRouting {
    /// Create a new IOMMU-aware IOAPIC routing wrapper.
    ///
    /// - `inner`: the hypervisor's IoApicRouting implementation
    /// - `rid`: IOAPIC RID (used for DTE/IRTE lookup)
    /// - `remapper`: the IOMMU's interrupt remapper
    pub fn new(
        inner: Arc<dyn IoApicRouting>,
        rid: u16,
        remapper: Arc<dyn InterruptRemapper>,
    ) -> Arc<Self> {
        let this = Arc::new(Self {
            inner,
            rid,
            remapper: remapper.clone(),
            raw_routes: Mutex::new([None; NUM_ENTRIES]),
        });
        remapper.register_route(&(this.clone() as Arc<dyn RetranslateInterrupts>));
        this
    }

    /// Translate and forward a single route to the inner implementation.
    fn translate_and_set(
        &self,
        irq: u8,
        raw: Option<MsiRequest>,
        remapper: &dyn InterruptRemapper,
    ) {
        let translated = raw.and_then(|r| {
            remapper
                .remap_msi(self.rid, r.address, r.data)
                .map(|(address, data)| MsiRequest { address, data })
        });
        self.inner.set_irq_route(irq, translated);
    }
}

impl IoApicRouting for IommuIoApicRouting {
    fn set_irq_route(&self, irq: u8, request: Option<MsiRequest>) {
        let mut routes = self.raw_routes.lock();
        if let Some(slot) = routes.get_mut(irq as usize) {
            *slot = request;
        }
        // Hold the lock across translate to serialize with retranslate().
        self.translate_and_set(irq, request, &*self.remapper);
    }

    fn assert_irq(&self, irq: u8) {
        // Route is already programmed in the hypervisor; just forward.
        self.inner.assert_irq(irq);
    }
}

impl RetranslateInterrupts for IommuIoApicRouting {
    fn device_id(&self) -> u16 {
        self.rid
    }

    fn retranslate(&self) {
        let routes = self.raw_routes.lock();
        for (irq, raw) in routes.iter().enumerate() {
            self.translate_and_set(irq as u8, *raw, &*self.remapper);
        }
    }
}
