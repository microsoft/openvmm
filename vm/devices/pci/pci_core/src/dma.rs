// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! DMA target for PCI devices.
//!
//! [`DmaTarget`] bundles a device's [`GuestMemory`] (for DMA reads/writes)
//! and [`MsiTarget`] (for MSI interrupt delivery) into a single type.
//!
//! In hardware, both DMA and MSI are bus-mastered transactions identified
//! by the device's Requester ID (RID). This type ensures the two always
//! carry a consistent device identity. For SR-IOV devices, calling
//! [`DmaTarget::with_devfn`] derives both DMA and MSI targets for a
//! specific VF in a single operation — you can't accidentally end up
//! with mismatched identities.

use crate::msi::MsiTarget;
use guestmem::GuestMemory;
use std::sync::Arc;

/// A trait for IOMMU backends that produce per-device guest memory.
///
/// Implemented by SMMU (and future VT-d, AMD-Vi, etc.). The factory is
/// shared across all devices behind the same IOMMU instance.
pub trait DmaTargetIommu: Send + Sync + 'static {
    /// Create a [`GuestMemory`] whose DMA translations use the IOMMU
    /// context for the given device/function number.
    ///
    /// The implementation composes the full Requester ID from its own
    /// bus range and the given `devfn`.
    fn guest_memory_for_devfn(&self, devfn: u8) -> GuestMemory;

    /// Create a [`GuestMemory`] for a specific Requester ID.
    ///
    /// `rid` is `(bus << 8) | devfn`. Use this when VFs span multiple
    /// bus numbers and the caller has computed the full RID from
    /// config space (e.g., from VF Offset / VF Stride and the
    /// assigned secondary bus).
    fn guest_memory_for_rid(&self, rid: u16) -> GuestMemory;
}

/// Everything a PCI device needs for bus-mastered transactions: DMA
/// memory access and MSI interrupt delivery.
///
/// Most devices only need [`guest_memory`](Self::guest_memory) and
/// [`msi_target`](Self::msi_target). SR-IOV PFs additionally call
/// [`with_devfn`](Self::with_devfn) when creating VFs.
#[derive(Clone)]
pub struct DmaTarget {
    guest_memory: GuestMemory,
    msi_target: MsiTarget,
    /// When an IOMMU is present, produces per-device GuestMemory
    /// instances with distinct stream/context table entries.
    iommu: Option<Arc<dyn DmaTargetIommu>>,
    /// Whether the device is behind a software IOMMU (e.g., emulated
    /// SMMU) that cannot program the host IOMMU for passthrough DMA.
    software_iommu: bool,
}

impl DmaTarget {
    /// Creates a DMA target with no IOMMU.
    ///
    /// All functions share the same guest memory, and `with_devfn`
    /// only updates the MSI identity.
    pub fn new(guest_memory: GuestMemory, msi_target: MsiTarget) -> Self {
        Self {
            guest_memory,
            msi_target,
            iommu: None,
            software_iommu: false,
        }
    }

    /// Creates a DMA target backed by an IOMMU.
    ///
    /// `guest_memory` is the default (function 0) DMA translation.
    /// `iommu` produces per-device translations for `with_devfn`.
    pub fn with_iommu(
        guest_memory: GuestMemory,
        msi_target: MsiTarget,
        iommu: Arc<dyn DmaTargetIommu>,
    ) -> Self {
        Self {
            guest_memory,
            msi_target,
            iommu: Some(iommu),
            software_iommu: true,
        }
    }

    /// Returns the guest memory for DMA from this device.
    pub fn guest_memory(&self) -> &GuestMemory {
        &self.guest_memory
    }

    /// Returns the MSI target for interrupt delivery from this device.
    pub fn msi_target(&self) -> &MsiTarget {
        &self.msi_target
    }

    /// Whether the device is behind a software IOMMU that cannot
    /// program the host IOMMU for passthrough DMA.
    pub fn software_iommu(&self) -> bool {
        self.software_iommu
    }

    /// Derives a DMA target for a different device function.
    ///
    /// `devfn` is the device/function number within the port's bus.
    /// Both DMA and MSI identity are updated atomically:
    /// - When an IOMMU is present, the returned target's guest memory
    ///   uses a different stream/context table entry. The IOMMU
    ///   implementation composes the full RID from its bus range
    ///   and the given `devfn`.
    /// - The MSI target is derived via [`MsiTarget::with_devfn`].
    ///
    /// When no IOMMU is present, only the MSI identity changes; the
    /// guest memory is shared (cloned).
    pub fn with_devfn(&self, devfn: u8) -> DmaTarget {
        DmaTarget {
            guest_memory: match &self.iommu {
                Some(factory) => factory.guest_memory_for_devfn(devfn),
                None => self.guest_memory.clone(),
            },
            msi_target: self.msi_target.with_devfn(devfn),
            iommu: self.iommu.clone(),
            software_iommu: self.software_iommu,
        }
    }

    /// Derives a DMA target for a specific Requester ID.
    ///
    /// `rid` is `(bus << 8) | devfn`. Use this when VFs span
    /// multiple bus numbers and the device has computed the full RID
    /// from config space assignments (secondary bus + VF Offset +
    /// VF Stride).
    ///
    /// Both DMA and MSI identity are updated atomically.
    pub fn with_rid(&self, rid: u16) -> DmaTarget {
        DmaTarget {
            guest_memory: match &self.iommu {
                Some(factory) => factory.guest_memory_for_rid(rid),
                None => self.guest_memory.clone(),
            },
            msi_target: self.msi_target.with_rid(rid),
            iommu: self.iommu.clone(),
            software_iommu: self.software_iommu,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus_range::AssignedBusRange;
    use crate::msi::MsiConnection;
    use crate::msi::SignalMsi;
    use parking_lot::Mutex;
    use std::sync::Arc;

    /// Records the requester IDs signaled through an `MsiTarget`, so tests
    /// can observe the MSI identity derived by `with_devfn` / `with_rid`.
    struct RecordingSignalMsi {
        calls: Mutex<Vec<Option<u32>>>,
    }

    impl RecordingSignalMsi {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
            })
        }

        fn pop(&self) -> Option<u32> {
            self.calls.lock().pop().flatten()
        }
    }

    impl SignalMsi for RecordingSignalMsi {
        fn signal_msi(&self, devid: Option<u32>, _address: u64, _data: u32) {
            self.calls.lock().push(devid);
        }
    }

    /// Records the `devfn` / `rid` passed to the IOMMU factory and hands
    /// back a distinct `GuestMemory` for each call so tests can confirm the
    /// derived target uses the IOMMU-provided memory.
    struct RecordingIommu {
        devfn_calls: Mutex<Vec<u8>>,
        rid_calls: Mutex<Vec<u16>>,
    }

    impl RecordingIommu {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                devfn_calls: Mutex::new(Vec::new()),
                rid_calls: Mutex::new(Vec::new()),
            })
        }
    }

    impl DmaTargetIommu for RecordingIommu {
        fn guest_memory_for_devfn(&self, devfn: u8) -> GuestMemory {
            self.devfn_calls.lock().push(devfn);
            // A distinct, non-empty allocation marks this as IOMMU-provided.
            GuestMemory::allocate(0x2000)
        }

        fn guest_memory_for_rid(&self, rid: u16) -> GuestMemory {
            self.rid_calls.lock().push(rid);
            GuestMemory::allocate(0x2000)
        }
    }

    #[test]
    fn new_has_no_iommu() {
        let target = DmaTarget::new(GuestMemory::empty(), MsiTarget::disconnected());
        assert!(!target.software_iommu());
        assert!(target.iommu.is_none());
    }

    #[test]
    fn with_devfn_no_iommu_shares_memory_and_updates_msi() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let msi_conn = MsiConnection::new(bus_range, 0);
        let recorder = RecordingSignalMsi::new();
        msi_conn.connect(recorder.clone());

        let gm = GuestMemory::allocate(0x1000);
        let target = DmaTarget::new(gm.clone(), msi_conn.target().clone());

        let derived = target.with_devfn(0x18); // dev 3, fn 0

        // No IOMMU: the guest memory is shared. Write through the original
        // and observe it through the derived target.
        target.guest_memory().write_at(0, &[0xAB]).unwrap();
        let mut buf = [0u8];
        derived.guest_memory().read_at(0, &mut buf).unwrap();
        assert_eq!(buf[0], 0xAB);

        // The MSI identity is derived from the devfn: bus 5 (secondary) | devfn.
        assert!(!derived.software_iommu());
        derived.msi_target().signal_msi(0xFEE0_0000, 0);
        assert_eq!(recorder.pop().unwrap(), (5 << 8) | 0x18);
    }

    #[test]
    fn with_devfn_iommu_derives_memory_and_msi_together() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let msi_conn = MsiConnection::new(bus_range, 0);
        let recorder = RecordingSignalMsi::new();
        msi_conn.connect(recorder.clone());

        let iommu = RecordingIommu::new();
        let target = DmaTarget::with_iommu(
            GuestMemory::empty(),
            msi_conn.target().clone(),
            iommu.clone(),
        );
        assert!(target.software_iommu());

        let derived = target.with_devfn(0x18);

        // The IOMMU factory was asked for the same devfn used for MSI.
        assert_eq!(*iommu.devfn_calls.lock(), vec![0x18]);
        // The derived target uses the IOMMU-provided 0x2000 allocation, not
        // the empty base memory: an access past the (empty) base succeeds.
        derived.guest_memory().write_at(0x1500, &[0xCD]).unwrap();
        let mut buf = [0u8];
        derived.guest_memory().read_at(0x1500, &mut buf).unwrap();
        assert_eq!(buf[0], 0xCD);
        assert!(derived.software_iommu());

        derived.msi_target().signal_msi(0xFEE0_0000, 0);
        assert_eq!(recorder.pop().unwrap(), (5 << 8) | 0x18);
    }

    #[test]
    fn with_rid_iommu_derives_memory_and_msi_together() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let msi_conn = MsiConnection::new(bus_range, 0);
        let recorder = RecordingSignalMsi::new();
        msi_conn.connect(recorder.clone());

        let iommu = RecordingIommu::new();
        let target = DmaTarget::with_iommu(
            GuestMemory::empty(),
            msi_conn.target().clone(),
            iommu.clone(),
        );

        let rid: u16 = (7 << 8) | 0x0A; // bus 7 (within [5, 10]), devfn 0x0A
        let derived = target.with_rid(rid);

        // The IOMMU factory was asked for the same RID used for MSI.
        assert_eq!(*iommu.rid_calls.lock(), vec![rid]);
        // The derived target uses the IOMMU-provided 0x2000 allocation, not
        // the empty base memory: an access past the (empty) base succeeds.
        derived.guest_memory().write_at(0x1500, &[0xCD]).unwrap();
        let mut buf = [0u8];
        derived.guest_memory().read_at(0x1500, &mut buf).unwrap();
        assert_eq!(buf[0], 0xCD);
        assert!(derived.software_iommu());

        derived.msi_target().signal_msi(0xFEE0_0000, 0);
        assert_eq!(recorder.pop().unwrap(), rid as u32);
    }
}
