// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for microVM chipset devices.

use mesh::MeshPayload;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vm_resource::kind::SerialBackendHandle;

/// The microVM bidirectional portb console at ports `0xe9` and `0xea`.
#[derive(MeshPayload)]
pub struct MicrovmPortbHandle {
    /// Host serial endpoint used for raw input and output.
    pub io: Resource<SerialBackendHandle>,
}

impl ResourceId<ChipsetDeviceHandleKind> for MicrovmPortbHandle {
    const ID: &'static str = "microvm-portb";
}

/// microVM shutdown control port at `0x604`.
#[derive(MeshPayload)]
pub struct MicrovmShutdownHandle;

impl ResourceId<ChipsetDeviceHandleKind> for MicrovmShutdownHandle {
    const ID: &'static str = "microvm-shutdown";
}
