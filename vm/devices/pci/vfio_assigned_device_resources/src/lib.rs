// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for VFIO-assigned PCI devices.

#![forbid(unsafe_code)]

use mesh::MeshPayload;
use vm_resource::ResourceId;
use vm_resource::kind::PciDeviceHandleKind;

/// A handle to a VFIO-assigned PCI device.
///
/// The resolver opens the VFIO container/group/device, configures the IOMMU,
/// maps guest RAM for DMA, and creates the device. Only the PCI BDF address
/// is needed in the handle — all runtime state is created during resolution.
#[derive(MeshPayload)]
pub struct VfioDeviceHandle {
    /// PCI BDF address on the host (e.g., "0000:3f:7a.0").
    pub pci_id: String,
}

impl ResourceId<PciDeviceHandleKind> for VfioDeviceHandle {
    const ID: &'static str = "vfio";
}
