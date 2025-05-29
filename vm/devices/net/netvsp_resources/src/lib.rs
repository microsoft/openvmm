// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for netvsp devices.

#![forbid(unsafe_code)]

use guid::Guid;
use mesh::MeshPayload;
use net_backend_resources::mac_address::MacAddress;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::NetEndpointHandleKind;
use vm_resource::kind::VmbusDeviceHandleKind;

/// A handle to a netvsp device.
#[derive(MeshPayload)]
pub struct NetvspHandle {
    /// The vmbus instance ID.
    pub instance_id: Guid,
    /// The NIC's mac address.
    pub mac_address: MacAddress,
    /// A handle to the backend endpoint.
    pub endpoint: Resource<NetEndpointHandleKind>,
    /// Optionally, the maximum number of queues to expose to the guest. This
    /// will be further limited by the backend endpoint.
    pub max_queues: Option<u16>,
}

impl ResourceId<VmbusDeviceHandleKind> for NetvspHandle {
    const ID: &'static str = "netvsp";
}
