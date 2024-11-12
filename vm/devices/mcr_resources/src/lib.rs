// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TODO MCR

#![forbid(unsafe_code)]
// #![warn(missing_docs)] // TODO MCR

use mesh::MeshPayload;
use vm_resource::kind::PciDeviceHandleKind;
use vm_resource::ResourceId;

#[derive(MeshPayload)]
pub struct McrControllerHandle {
    pub instance_id: guid::Guid,
}

impl ResourceId<PciDeviceHandleKind> for McrControllerHandle {
    const ID: &'static str = "mcr";
}
