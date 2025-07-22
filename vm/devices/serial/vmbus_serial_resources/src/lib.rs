// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for vmbus serial ports.

#![forbid(unsafe_code)]

use mesh::MeshPayload;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::SerialBackendHandle;
use vm_resource::kind::VmbusDeviceHandleKind;

/// A handle to a vmbus serial device.
#[derive(MeshPayload)]
pub struct VmbusSerialDeviceHandle {
    /// The port identity within the guest.
    pub port: VmbusSerialPort,
    /// The serial port backend.
    pub backend: Resource<SerialBackendHandle>,
}

impl ResourceId<VmbusDeviceHandleKind> for VmbusSerialDeviceHandle {
    const ID: &'static str = "vmbus_serial";
}

/// The port identity. This corresponds to different specific vmbus instance
/// IDs.
#[derive(MeshPayload)]
pub enum VmbusSerialPort {
    /// A device to reemulate as "COM1".
    Com1,
    /// A device to reemulate as "COM2".
    Com2,
}
