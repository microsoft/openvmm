// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for NVMe controllers.

#![forbid(unsafe_code)]

use guid::Guid;
use mesh::MeshPayload;
use vm_resource::kind::DiskHandleKind;
use vm_resource::kind::PciDeviceHandleKind;
use vm_resource::Resource;
use vm_resource::ResourceId;

/// A handle to an NVMe controller.
#[derive(MeshPayload)]
pub struct NvmeControllerHandle {
    /// The subsystem ID to use when responding to controller identify queries.
    pub subsystem_id: Guid,
    /// The number of MSI-X interrupts to support.
    pub msix_count: u16,
    /// The number of IO queues to support.
    pub max_io_queues: u16,
    /// The initial set of namespaces.
    pub namespaces: Vec<NamespaceDefinition>,
}

impl ResourceId<PciDeviceHandleKind> for NvmeControllerHandle {
    const ID: &'static str = "nvme";
}

/// A controller namespace definition.
#[derive(MeshPayload)]
pub struct NamespaceDefinition {
    /// The namespace ID.
    pub nsid: u32,
    /// Whether the disk is read only.
    pub read_only: bool,
    /// The backing disk resource.
    pub disk: Resource<DiskHandleKind>,
}
