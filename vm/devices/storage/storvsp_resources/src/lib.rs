// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for storvsp.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use guid::Guid;
use mesh::payload::Protobuf;
use mesh::rpc::FailableRpc;
use mesh::MeshPayload;
use vm_resource::kind::ScsiDeviceHandleKind;
use vm_resource::kind::VmbusDeviceHandleKind;
use vm_resource::Resource;
use vm_resource::ResourceId;

/// A path at which to enumerate a SCSI logical unit.
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq, Hash, Protobuf)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct ScsiPath {
    /// The SCSI path number.
    pub path: u8,
    /// The SCSI target number.
    pub target: u8,
    /// The SCSI LUN.
    pub lun: u8,
}

impl std::fmt::Display for ScsiPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.path, self.target, self.lun)
    }
}

/// Handle for a storvsp SCSI controller device.
#[derive(MeshPayload)]
pub struct ScsiControllerHandle {
    /// The VMBus instance ID.
    pub instance_id: Guid,
    /// The maximum IO queue depth per channel.
    pub io_queue_depth: Option<u32>,
    /// The maximum number of subchannels (so the maximum number of channels
    /// minus one).
    pub max_sub_channel_count: u16,
    /// The initial set of SCSI devices.
    pub devices: Vec<ScsiDeviceAndPath>,
    /// Runtime request channel.
    pub requests: Option<mesh::Receiver<ScsiControllerRequest>>,
}

impl ResourceId<VmbusDeviceHandleKind> for ScsiControllerHandle {
    const ID: &'static str = "scsi";
}

/// A SCSI device resource handle and associated path.
#[derive(MeshPayload)]
pub struct ScsiDeviceAndPath {
    /// The path to the device.
    pub path: ScsiPath,
    /// The device resource.
    pub device: Resource<ScsiDeviceHandleKind>,
}

/// A runtime request to the SCSI controller.
#[derive(MeshPayload)]
pub enum ScsiControllerRequest {
    /// Add a device.
    AddDevice(FailableRpc<ScsiDeviceAndPath, ()>),
    /// Remove a device.
    RemoveDevice(FailableRpc<ScsiPath, ()>),
}
