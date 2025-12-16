// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resources for remote chipset devices.

#![forbid(unsafe_code)]

use mesh::MeshPayload;
use mesh_worker::WorkerHost;
use vm_resource::Resource;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// A handle to construct a chipset device in a remote process.
#[derive(MeshPayload)]
pub struct RemoteChipsetDeviceHandle {
    /// The device to run in the worker.
    pub device: Resource<ChipsetDeviceHandleKind>,
    /// The worker host to launch the worker in.
    pub worker_host: WorkerHost,
}

impl vm_resource::ResourceId<ChipsetDeviceHandleKind> for RemoteChipsetDeviceHandle {
    const ID: &'static str = "chipset_device_worker_handle";
}
