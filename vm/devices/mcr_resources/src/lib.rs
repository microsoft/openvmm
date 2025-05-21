// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TODO MCR

#![expect(missing_docs)]
#![forbid(unsafe_code)]

use mesh::MeshPayload;
use vm_resource::ResourceId;
use vm_resource::kind::PciDeviceHandleKind;

#[derive(MeshPayload)]
pub struct McrControllerHandle {
    pub instance_id: guid::Guid,
}

impl ResourceId<PciDeviceHandleKind> for McrControllerHandle {
    const ID: &'static str = "mcr";
}
