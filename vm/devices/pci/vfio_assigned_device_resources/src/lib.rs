// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for VFIO-assigned PCI devices.

#![forbid(unsafe_code)]

use mesh::MeshPayload;
use std::fs::File;
use vm_resource::ResourceId;
use vm_resource::kind::PciDeviceHandleKind;

/// How a virtual BAR should be pre-programmed before the guest configures it.
#[derive(Copy, Clone, Debug, Default, Eq, MeshPayload, PartialEq)]
pub enum BarAddressConfig {
    /// Do not pre-program this BAR; let the guest assign it normally.
    #[default]
    GuestAssigned,
    /// Use the physical BAR address reported by the host.
    HostAssigned,
    /// Use an explicitly supplied host physical address.
    Fixed(u64),
}

/// A handle to a VFIO-assigned PCI device (legacy group path).
///
/// The launcher opens the VFIO group file descriptor (e.g., `/dev/vfio/N`)
/// and passes it here so that the VMM process does not need direct access
/// to `/dev/vfio/` or sysfs.
#[derive(MeshPayload)]
pub struct VfioDeviceHandle {
    /// PCI BDF address on the host (e.g., "0000:3f:7a.0").
    pub pci_id: String,
    /// Pre-opened VFIO group file descriptor (`/dev/vfio/<group_id>`).
    pub group: File,
    /// Per-BAR pre-programming configuration.
    pub bar_addresses: [BarAddressConfig; 6],
}

impl ResourceId<PciDeviceHandleKind> for VfioDeviceHandle {
    const ID: &'static str = "vfio";
}

/// A handle to a VFIO-assigned PCI device (cdev + iommufd path).
///
/// The launcher opens the VFIO cdev file descriptor
/// (e.g., `/dev/vfio/devices/vfio0`) and the iommufd file descriptor
/// (`/dev/iommu`) and passes them here. The VMM binds the device to the
/// iommufd instance and attaches an IOAS for DMA mapping.
#[derive(MeshPayload)]
pub struct VfioCdevDeviceHandle {
    /// PCI BDF address on the host (e.g., "0000:3f:7a.0").
    pub pci_id: String,
    /// Pre-opened VFIO cdev file descriptor (`/dev/vfio/devices/vfioN`).
    pub cdev: File,
    /// Pre-opened iommufd file descriptor (`/dev/iommu`).
    pub iommufd: File,
    /// The `--iommu` context ID this device belongs to. All devices
    /// sharing the same ID share a single IOAS (one set of page tables).
    pub iommu_id: String,
    /// Per-BAR pre-programming configuration.
    pub bar_addresses: [BarAddressConfig; 6],
}

impl ResourceId<PciDeviceHandleKind> for VfioCdevDeviceHandle {
    const ID: &'static str = "vfio-cdev";
}
