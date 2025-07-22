// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for the timesync IC.

use mesh::MeshPayload;
use vm_resource::ResourceId;
use vm_resource::kind::VmbusDeviceHandleKind;

/// A handle to the timesync IC.
#[derive(MeshPayload)]
pub struct TimesyncIcHandle;

impl ResourceId<VmbusDeviceHandleKind> for TimesyncIcHandle {
    const ID: &'static str = "timesync_ic";
}
