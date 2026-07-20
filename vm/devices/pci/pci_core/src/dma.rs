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
use std::any::Any;
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

/// How a device's DMA relates to host passthrough (VFIO assignment).
///
/// This is the only IOMMU-related view exposed to consumers of
/// [`DmaTarget`]. It deliberately says nothing about how guest memory is
/// translated (that is the underlying [`DmaTargetIommu`] factory); it only
/// answers "can this device be handed to the host for passthrough, and if
/// so, how does the assignment backend attach to the IOMMU?".
pub enum DmaPassthrough<'a> {
    /// No IOMMU, or none relevant to passthrough — host assignment allowed.
    Allowed,
    /// Behind a software/emulated IOMMU that cannot program the host IOMMU
    /// for passthrough DMA — host assignment rejected.
    SoftwareBlocked,
    /// Behind a hardware-nestable IOMMU — host assignment allowed. The
    /// opaque handle lets the assignment backend (which knows the concrete
    /// IOMMU type) downcast and attach to the emulated IOMMU for nested
    /// stage-1 translation.
    HardwareNestable(&'a (dyn Any + Send + Sync)),
}

/// Private representation of a [`DmaTarget`]'s passthrough disposition.
#[derive(Clone)]
enum Passthrough {
    /// Host assignment allowed (no IOMMU, or none relevant).
    Allowed,
    /// Behind a software/emulated IOMMU — host assignment rejected.
    SoftwareBlocked,
    /// Behind a hardware-nestable IOMMU — the opaque handle is downcast by
    /// the assignment backend to its arch-specific nesting context.
    HardwareNestable(Arc<dyn Any + Send + Sync>),
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
    /// The device's host-passthrough disposition (see [`DmaPassthrough`]).
    passthrough: Passthrough,
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
            passthrough: Passthrough::Allowed,
        }
    }

    /// Creates a DMA target backed by a software/emulated IOMMU.
    ///
    /// The base (function-`devfn`) translating guest memory is derived from
    /// `iommu`; per-VF memory is produced by
    /// [`with_rid_offset`](Self::with_rid_offset). The MSI backend is taken
    /// (late-bound) from `msi`.
    ///
    /// The device is marked [`DmaPassthrough::SoftwareBlocked`]: a
    /// software/emulated IOMMU cannot program the host IOMMU, so host
    /// passthrough (VFIO assignment) is rejected. Use
    /// [`with_nestable_iommu`](Self::with_nestable_iommu) for a
    /// hardware-nestable IOMMU that permits passthrough.
    pub fn with_iommu(
        bus_range: AssignedBusRange,
        devfn: u8,
        iommu: Arc<dyn DmaTargetIommu>,
        msi: &MsiConnection,
    ) -> Self {
        Self::iommu_backed(bus_range, devfn, iommu, msi, Passthrough::SoftwareBlocked)
    }

    /// Creates a DMA target backed by a hardware-nestable IOMMU.
    ///
    /// Like [`with_iommu`](Self::with_iommu), but marks the device
    /// [`DmaPassthrough::HardwareNestable`] with an opaque `handle` that the
    /// host-assignment backend downcasts to its arch-specific nesting context.
    /// Accel-capable IOMMUs (e.g. an SMMU that programs the host IOMMU for
    /// nested stage-1 translation) use this to permit VFIO passthrough despite
    /// wrapping guest memory with a translating target; the `handle` carries
    /// everything the backend needs to wire the device into the emulated IOMMU.
    ///
    /// Supplying the nesting `handle` together with the `iommu` is what makes a
    /// [`DmaPassthrough::HardwareNestable`] target impossible to construct
    /// without a backing IOMMU.
    pub fn with_nestable_iommu(
        bus_range: AssignedBusRange,
        devfn: u8,
        iommu: Arc<dyn DmaTargetIommu>,
        handle: Arc<dyn Any + Send + Sync>,
        msi: &MsiConnection,
    ) -> Self {
        Self::iommu_backed(
            bus_range,
            devfn,
            iommu,
            msi,
            Passthrough::HardwareNestable(handle),
        )
    }

    /// Shared constructor for the IOMMU-backed cases: derives the base
    /// translating memory and MSI identity, then stamps the passthrough
    /// disposition.
    fn iommu_backed(
        bus_range: AssignedBusRange,
        devfn: u8,
        iommu: Arc<dyn DmaTargetIommu>,
        msi: &MsiConnection,
        passthrough: Passthrough,
    ) -> Self {
        let guest_memory = iommu.guest_memory_for_rid_offset(devfn as u16);
        let msi_target = msi.msi_target(bus_range, devfn);
        Self {
            rid_offset: devfn as u16,
            guest_memory,
            msi_target,
            iommu: Some(iommu),
            passthrough,
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

    /// The device's host-passthrough disposition.
    ///
    /// Used by host-assignment backends (e.g. the VFIO resolver) to decide
    /// whether the device may be passed through and, when it is behind a
    /// hardware-nestable IOMMU, to obtain the opaque handle they downcast to
    /// attach to the emulated IOMMU.
    pub fn passthrough(&self) -> DmaPassthrough<'_> {
        match &self.passthrough {
            Passthrough::Allowed => DmaPassthrough::Allowed,
            Passthrough::SoftwareBlocked => DmaPassthrough::SoftwareBlocked,
            Passthrough::HardwareNestable(handle) => {
                DmaPassthrough::HardwareNestable(handle.as_ref())
            }
        }
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
            passthrough: self.passthrough.clone(),
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
        assert!(matches!(target.passthrough(), DmaPassthrough::Allowed));
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
        assert!(matches!(derived.passthrough(), DmaPassthrough::Allowed));
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
        assert!(matches!(
            target.passthrough(),
            DmaPassthrough::SoftwareBlocked
        ));

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
        assert!(matches!(
            derived.passthrough(),
            DmaPassthrough::SoftwareBlocked
        ));

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
        assert!(matches!(
            derived.passthrough(),
            DmaPassthrough::SoftwareBlocked
        ));

        derived.msi_target().signal_msi(0xFEE0_0000, 0);
        assert_eq!(recorder.pop().unwrap(), (7 << 8) | 0x0A);
    }

    /// An opaque nesting context behind the `HardwareNestable` handle, of the
    /// kind an IOMMU backend crate would define.
    #[derive(Debug, PartialEq)]
    struct FakeNestingContext {
        id: u32,
    }

    #[test]
    fn with_nestable_iommu_exposes_downcastable_handle() {
        let bus_range = AssignedBusRange::new();
        bus_range.set_bus_range(5, 10);
        let msi_conn = MsiConnection::new();

        let iommu = RecordingIommu::new();
        let target = DmaTarget::with_nestable_iommu(
            bus_range.clone(),
            0,
            iommu,
            Arc::new(FakeNestingContext { id: 0x1234 }),
            &msi_conn,
        );

        // The base target accepts passthrough and hands back the context.
        match target.passthrough() {
            DmaPassthrough::HardwareNestable(handle) => {
                let ctx = handle.downcast_ref::<FakeNestingContext>().unwrap();
                assert_eq!(ctx, &FakeNestingContext { id: 0x1234 });
            }
            _ => panic!("expected HardwareNestable"),
        }

        // The nesting disposition (and handle) survives VF derivation.
        let derived = target.with_rid_offset(0x18);
        match derived.passthrough() {
            DmaPassthrough::HardwareNestable(handle) => {
                assert_eq!(
                    handle.downcast_ref::<FakeNestingContext>().unwrap().id,
                    0x1234
                );
            }
            _ => panic!("expected HardwareNestable on derived target"),
        }
    }
}
