// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resources for the vmbfs device.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

use mesh::MeshPayload;
use std::fs::File;
use vm_resource::kind::VmbusDeviceHandleKind;
use vm_resource::ResourceId;

/// A handle to a vmbfs device for providing an IMC hive to the Windows boot
/// loader.
#[derive(MeshPayload)]
pub struct VmbfsImcDeviceHandle {
    /// The file containing the IMC hive data.
    pub file: File,
}

impl ResourceId<VmbusDeviceHandleKind> for VmbfsImcDeviceHandle {
    const ID: &'static str = "vmbfs-imc";
}
