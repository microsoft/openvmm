// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for the IPMI KCS device.

#![forbid(unsafe_code)]

use mesh::MeshPayload;
use vm_resource::ResourceId;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// Resource handle for the IPMI KCS device.
///
/// No configuration fields — the device starts with an empty SEL
/// and the guest populates it at runtime.
#[derive(MeshPayload)]
pub struct IpmiKcsHandle;

impl ResourceId<ChipsetDeviceHandleKind> for IpmiKcsHandle {
    const ID: &'static str = "ipmi_kcs";
}
