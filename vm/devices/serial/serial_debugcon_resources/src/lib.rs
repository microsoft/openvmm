// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for the debugcon serial device.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

use mesh::MeshPayload;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vm_resource::kind::SerialBackendHandle;
use vm_resource::Resource;
use vm_resource::ResourceId;

/// A handle to a 16550A serial device.
#[derive(MeshPayload)]
pub struct SerialDebugconDeviceHandle {
    /// Which IO port to put the single-byte debugcon register.
    pub port: u16,
    /// The IO backend.
    pub io: Resource<SerialBackendHandle>,
}

impl ResourceId<ChipsetDeviceHandleKind> for SerialDebugconDeviceHandle {
    const ID: &'static str = "debugcon";
}
