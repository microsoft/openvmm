// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for NVMe controllers.

#![forbid(unsafe_code)]

use crate::fault::FaultConfiguration;
use guid::Guid;
use mesh::MeshPayload;
use mesh::rpc::FailableRpc;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::DiskHandleKind;
use vm_resource::kind::PciDeviceHandleKind;

pub mod fault;

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
    /// Runtime request channel for hot add/remove of namespaces.
    pub requests: Option<mesh::Receiver<NvmeControllerRequest>>,
}

impl ResourceId<PciDeviceHandleKind> for NvmeControllerHandle {
    const ID: &'static str = "nvme";
}

/// A runtime request to the NVMe controller.
#[derive(MeshPayload)]
pub enum NvmeControllerRequest {
    /// Add a namespace.
    AddNamespace(FailableRpc<NamespaceDefinition, ()>),
    /// Remove a namespace by its NSID.
    RemoveNamespace(FailableRpc<u32, ()>),
}

/// A handle to a NVMe fault controller.
#[derive(MeshPayload)]
pub struct NvmeFaultControllerHandle {
    /// The subsystem ID to use when responding to controller identify queries.
    pub subsystem_id: Guid,
    /// The number of MSI-X interrupts to support.
    pub msix_count: u16,
    /// The number of IO queues to support.
    pub max_io_queues: u16,
    /// The initial set of namespaces.
    pub namespaces: Vec<NamespaceDefinition>,
    /// Configuration for the fault
    pub fault_config: FaultConfiguration,
    /// Enable TDISP testing on this device when presented by a TDISP host.
    pub enable_tdisp_tests: bool,
}

impl ResourceId<PciDeviceHandleKind> for NvmeFaultControllerHandle {
    const ID: &'static str = "nvme_fault";
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
