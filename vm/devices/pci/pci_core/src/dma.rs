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
