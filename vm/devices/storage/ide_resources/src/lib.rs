// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Client definitions for describing IDE controller configuration.
//!
//! TODO: refactor to support `Resource`-based instantiation of IDE controllers,
//! at which point this crate name makes sense.

#![forbid(unsafe_code)]

use inspect::Inspect;
use mesh::MeshPayload;
use vm_resource::kind::DiskHandleKind;
use vm_resource::kind::ScsiDeviceHandleKind;
use vm_resource::Resource;

/// The location of an IDE device on a controller.
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq, Hash, MeshPayload, Inspect)]
#[inspect(display)]
pub struct IdePath {
    /// The channel number. Must be zero or one.
    pub channel: u8,
    /// The device number on the channel. Must be zero or one.
    pub drive: u8,
}

impl std::fmt::Display for IdePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.channel, self.drive)
    }
}

/// Guest media for an IDE device.
#[derive(Debug, MeshPayload)]
pub enum GuestMedia {
    /// An ATAPI drive, backed by a SCSI device.
    Dvd(Resource<ScsiDeviceHandleKind>),
    /// An ATA disk, backed by a disk.
    Disk {
        /// The backing disk.
        disk_type: Resource<DiskHandleKind>,
        /// Whether the disk is read-only.
        read_only: bool,
        /// The disk parameters, used for the vmbus SCSI interface.
        disk_parameters: Option<scsidisk_resources::DiskParameters>,
    },
}

/// IDE device configuration.
#[derive(Debug, MeshPayload)]
pub struct IdeDeviceConfig {
    /// The location of the device on the controller.
    pub path: IdePath,
    /// The backing media for the device.
    pub guest_media: GuestMedia,
}

/// IDE controller configuration.
#[derive(Debug, MeshPayload)]
pub struct IdeControllerConfig {
    /// Disks on the primary channel.
    pub primary_channel_disks: Vec<IdeDeviceConfig>,
    /// Disks on the secondary channel.
    pub secondary_channel_disks: Vec<IdeDeviceConfig>,
    /// The maximum queue depth for the vmbus SCSI interface.
    pub io_queue_depth: Option<u32>,
}
