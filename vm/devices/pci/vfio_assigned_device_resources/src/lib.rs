// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for VFIO-assigned PCI devices.

#![forbid(unsafe_code)]

use mesh::MeshPayload;
use std::fs::File;
use vm_resource::ResourceId;
use vm_resource::kind::PciDeviceHandleKind;

/// A handle to a VFIO-assigned PCI device.
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
}

impl ResourceId<PciDeviceHandleKind> for VfioDeviceHandle {
    const ID: &'static str = "vfio";
}
