// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for MANA/GDMA devices.

#![forbid(unsafe_code)]

use mesh::MeshPayload;
use net_backend_resources::mac_address::MacAddress;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::NetEndpointHandleKind;
use vm_resource::kind::PciDeviceHandleKind;

/// A resource handle to a GDMA device.
#[derive(MeshPayload)]
pub struct GdmaDeviceHandle {
    /// The vports to instantiate on the NIC.
    pub vports: Vec<VportDefinition>,
}

impl ResourceId<PciDeviceHandleKind> for GdmaDeviceHandle {
    const ID: &'static str = "gdma";
}

/// A resource handle to a test-controllable GDMA device.
///
/// Used in VMM tests to issue typed hardware requests.
#[derive(MeshPayload)]
pub struct GdmaTestDeviceHandle {
    /// The vports to instantiate on the NIC.
    pub vports: Vec<VportDefinition>,
    /// Channel for delivering requests from the test harness.
    pub request_recv: mesh::Receiver<mesh::rpc::Rpc<GdmaTestRequest, ()>>,
}

impl ResourceId<PciDeviceHandleKind> for GdmaTestDeviceHandle {
    const ID: &'static str = "gdma-test";
}

/// A test request for an emulated GDMA device.
#[derive(MeshPayload)]
pub enum GdmaTestRequest {
    /// Request that the VF be reconfigured.
    VfReset {
        /// Whether OpenHCL should revoke the VTL0 VF during reset.
        revoke_vtl0_vf: bool,
    },
    /// Change a vport's link state.
    VportLinkState {
        /// The zero-based vport index.
        vport: u32,
        /// Whether the link should be connected.
        connected: bool,
    },
}

/// A basic NIC vport definition.
#[derive(MeshPayload)]
pub struct VportDefinition {
    /// The vport's MAC address.
    pub mac_address: MacAddress,
    /// The backend network endpoint for the vport.
    pub endpoint: Resource<NetEndpointHandleKind>,
}
