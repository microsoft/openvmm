// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for storvsp.

#![forbid(unsafe_code)]

use guid::Guid;
use mesh::MeshPayload;
use mesh::payload::Protobuf;
use mesh::rpc::FailableRpc;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::ScsiDeviceHandleKind;
use vm_resource::kind::VmbusDeviceHandleKind;

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
    /// Poll mode queue depth. To reduce jitter, storvsp will avoid unmasking the vmbus interrupt (to the guest) if
    /// there are IOs outstanding to any of the disks backing a device attached to that storvsp controller. This
    /// controls the number of outstanding IOs that trigger when to switch between masking interrupts and just
    /// assuming that some other activity will trigger a check of the queue.
    ///
    /// Higher numbers mean that there must be _more_ IOs outstanding to backing storage devices before storvsp
    /// decides to keep interrupts masked.
    pub poll_mode_queue_depth: Option<u32>,
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
