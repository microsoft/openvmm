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
//! [`DmaTarget::with_rid_offset`] derives both DMA and MSI targets for a
//! specific VF in a single operation — you can't accidentally end up
//! with mismatched identities.

use crate::bus_range::AssignedBusRange;
use crate::msi::MsiConnection;
use crate::msi::MsiTarget;
use guestmem::GuestMemory;
use std::sync::Arc;

/// A trait for IOMMU backends that produce per-device guest memory.
///
/// Implemented by SMMU (and future VT-d, AMD-Vi, etc.). The factory is
/// shared across all devices behind the same IOMMU instance.
pub trait DmaTargetIommu: Send + Sync + 'static {
    /// Create a [`GuestMemory`] for a requester-ID offset relative to the
    /// device's secondary bus.
    ///
    /// The RID is resolved as `(secondary << 8) + rid_offset` on each access,
    /// so it tracks the live bus assignment. A plain `devfn` is just an
    /// offset in `0..=0xff`; SR-IOV VFs use larger offsets that carry into
    /// the bus byte.
    fn guest_memory_for_rid_offset(&self, rid_offset: u16) -> GuestMemory;
}

/// Everything a PCI device needs for bus-mastered transactions: DMA
/// memory access and MSI interrupt delivery.
///
/// Most devices only need [`guest_memory`](Self::guest_memory) and
/// [`msi_target`](Self::msi_target). SR-IOV PFs additionally call
/// [`with_rid_offset`](Self::with_rid_offset) when creating VFs.
#[derive(Clone)]
pub struct DmaTarget {
    /// This target's requester-ID offset from the secondary bus. Held so
    /// [`with_rid_offset`](Self::with_rid_offset) can derive a target at a
    /// further offset relative to this one.
    rid_offset: u16,
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
    /// `bus_range` and `devfn` set the device's requester-ID identity, shared
    /// by both the DMA and MSI sides. The MSI backend is taken (late-bound)
    /// from `msi`. Since there is no IOMMU, all targets derived from this one
    /// share the same guest memory; [`with_rid_offset`](Self::with_rid_offset)
    /// only updates the MSI identity.
    pub fn new(
        bus_range: AssignedBusRange,
        devfn: u8,
        guest_memory: GuestMemory,
        msi: &MsiConnection,
    ) -> Self {
        let msi_target = msi.msi_target(bus_range, devfn);
        Self {
            rid_offset: devfn as u16,
            guest_memory,
            msi_target,
            iommu: None,
            software_iommu: false,
        }
    }

    /// Creates a DMA target backed by an IOMMU.
    ///
    /// The base (function-`devfn`) translating guest memory is derived from
    /// `iommu`; per-VF memory is produced by
    /// [`with_rid_offset`](Self::with_rid_offset). The MSI backend is taken
    /// (late-bound) from `msi`.
    pub fn with_iommu(
        bus_range: AssignedBusRange,
        devfn: u8,
        iommu: Arc<dyn DmaTargetIommu>,
        msi: &MsiConnection,
    ) -> Self {
        let guest_memory = iommu.guest_memory_for_rid_offset(devfn as u16);
        let msi_target = msi.msi_target(bus_range, devfn);
        Self {
            rid_offset: devfn as u16,
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

    /// Derives a DMA target offset by `delta` from this one in RID space.
    ///
    /// This is the SR-IOV VF derivation primitive: given a PF's target, its
    /// `i`th VF is `delta = VF_Offset + i * VF_Stride` away. Offsets stack,
    /// so deriving from an already-derived target accumulates. Both the DMA
    /// and MSI identity are derived in lockstep and resolved at use time as
    /// `(secondary << 8) + offset` against the live bus assignment, so VF
    /// targets can be derived before the bus is programmed.
    pub fn with_rid_offset(&self, delta: u16) -> DmaTarget {
        let rid_offset = self.rid_offset.wrapping_add(delta);
        DmaTarget {
            rid_offset,
            guest_memory: match &self.iommu {
                Some(factory) => factory.guest_memory_for_rid_offset(rid_offset),
                None => self.guest_memory.clone(),
            },
            msi_target: self.msi_target.with_rid_offset(rid_offset),
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
    /// can observe the MSI identity derived by `with_rid_offset`.
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

    /// Records the `rid_offset` passed to the IOMMU factory and hands back a
    /// distinct `GuestMemory` for each call so tests can confirm the derived
    /// target uses the IOMMU-provided memory.
    struct RecordingIommu {
        rid_offset_calls: Mutex<Vec<u16>>,
    }

    impl RecordingIommu {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                rid_offset_calls: Mutex::new(Vec::new()),
            })
        }
    }

    impl DmaTargetIommu for RecordingIommu {
        fn guest_memory_for_rid_offset(&self, rid_offset: u16) -> GuestMemory {
            self.rid_offset_calls.lock().push(rid_offset);
            // A distinct, non-empty allocation marks this as IOMMU-provided.
            GuestMemory::allocate(0x2000)
        }
    }

    #[test]
    fn new_has_no_iommu() {
        let msi_conn = MsiConnection::new();
        let target = DmaTarget::new(AssignedBusRange::new(), 0, GuestMemory::empty(), &msi_conn);
        assert!(!target.software_iommu());
        assert!(target.iommu.is_none());
    }

    #[test]
    fn with_rid_offset_no_iommu_shares_memory_and_updates_msi() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let msi_conn = MsiConnection::new();
        let recorder = RecordingSignalMsi::new();
        msi_conn.connect(recorder.clone());

        let gm = GuestMemory::allocate(0x1000);
        let target = DmaTarget::new(bus_range.clone(), 0, gm.clone(), &msi_conn);

        let derived = target.with_rid_offset(0x18); // function 0x18 on the secondary bus

        // No IOMMU: the guest memory is shared. Write through the original
        // and observe it through the derived target.
        target.guest_memory().write_at(0, &[0xAB]).unwrap();
        let mut buf = [0u8];
        derived.guest_memory().read_at(0, &mut buf).unwrap();
        assert_eq!(buf[0], 0xAB);

        // The MSI identity is derived from the offset: bus 5 (secondary) | offset.
        assert!(!derived.software_iommu());
        derived.msi_target().signal_msi(0xFEE0_0000, 0);
        assert_eq!(recorder.pop().unwrap(), (5 << 8) | 0x18);
    }

    #[test]
    fn with_rid_offset_stacks() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let msi_conn = MsiConnection::new();
        let recorder = RecordingSignalMsi::new();
        msi_conn.connect(recorder.clone());

        let target = DmaTarget::new(bus_range.clone(), 0, GuestMemory::empty(), &msi_conn);

        // Offsets accumulate: 0x10 then 0x08 lands at 0x18.
        let derived = target.with_rid_offset(0x10).with_rid_offset(0x08);
        derived.msi_target().signal_msi(0xFEE0_0000, 0);
        assert_eq!(recorder.pop().unwrap(), (5 << 8) | 0x18);
    }

    #[test]
    fn with_rid_offset_iommu_derives_memory_and_msi_together() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let msi_conn = MsiConnection::new();
        let recorder = RecordingSignalMsi::new();
        msi_conn.connect(recorder.clone());

        let iommu = RecordingIommu::new();
        let target = DmaTarget::with_iommu(bus_range.clone(), 0, iommu.clone(), &msi_conn);
        assert!(target.software_iommu());

        let derived = target.with_rid_offset(0x18);

        // The base target asked the factory for offset 0 (devfn 0); deriving
        // offset 0x18 asks for that offset.
        assert_eq!(*iommu.rid_offset_calls.lock(), vec![0, 0x18]);
        // The derived target uses the IOMMU-provided 0x2000 allocation: an
        // access past the empty base memory succeeds.
        derived.guest_memory().write_at(0x1500, &[0xCD]).unwrap();
        let mut buf = [0u8];
        derived.guest_memory().read_at(0x1500, &mut buf).unwrap();
        assert_eq!(buf[0], 0xCD);
        assert!(derived.software_iommu());

        derived.msi_target().signal_msi(0xFEE0_0000, 0);
        assert_eq!(recorder.pop().unwrap(), (5 << 8) | 0x18);
    }

    #[test]
    fn with_rid_offset_iommu_cross_bus() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let msi_conn = MsiConnection::new();
        let recorder = RecordingSignalMsi::new();
        msi_conn.connect(recorder.clone());

        let iommu = RecordingIommu::new();
        let target = DmaTarget::with_iommu(bus_range.clone(), 0, iommu.clone(), &msi_conn);

        // Offset (2 << 8) | 0x0A lands on bus 7 (secondary 5 + 2), devfn 0x0A.
        let offset: u16 = (2 << 8) | 0x0A;
        let derived = target.with_rid_offset(offset);

        // The base construction asked for offset 0 first, then the VF offset.
        assert_eq!(*iommu.rid_offset_calls.lock(), vec![0, offset]);
        // The derived target uses the IOMMU-provided 0x2000 allocation: an
        // access past the empty base memory succeeds.
        derived.guest_memory().write_at(0x1500, &[0xCD]).unwrap();
        let mut buf = [0u8];
        derived.guest_memory().read_at(0x1500, &mut buf).unwrap();
        assert_eq!(buf[0], 0xCD);
        assert!(derived.software_iommu());

        derived.msi_target().signal_msi(0xFEE0_0000, 0);
        assert_eq!(recorder.pop().unwrap(), (7 << 8) | 0x0A);
    }
}
