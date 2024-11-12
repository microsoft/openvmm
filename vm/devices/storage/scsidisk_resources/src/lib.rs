// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resources for emulated SCSI disks.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use inspect::Inspect;
use mesh::payload::Protobuf;
use mesh::rpc::FailableRpc;
use mesh::MeshPayload;
use storage_string::AsciiString;
use vm_resource::kind::DiskHandleKind;
use vm_resource::kind::ScsiDeviceHandleKind;
use vm_resource::Resource;
use vm_resource::ResourceId;

/// Resource handle for an emulated SCSI disk.
#[derive(MeshPayload)]
pub struct SimpleScsiDiskHandle {
    /// The backing simple disk handle.
    pub disk: Resource<DiskHandleKind>,
    /// Whether the disk is read only.
    pub read_only: bool,
    /// Parameters controlling how the SCSI emulation behaves.
    pub parameters: DiskParameters,
}

impl ResourceId<ScsiDeviceHandleKind> for SimpleScsiDiskHandle {
    const ID: &'static str = "emulated_disk";
}

/// Parameters controlling SCSI disk behavior.
///
/// These parameters are all optional. If not provided, a default will be chosen
/// based on the backing disk's capabilities.
#[derive(Debug, Default, Clone, Protobuf)]
pub struct DiskParameters {
    /// The disk ID, used in T10 identification.
    pub disk_id: Option<[u8; 16]>,
    /// Vendor/model disk information.
    pub identity: Option<DiskIdentity>,
    /// The disk's serial number.
    pub serial_number: Vec<u8>,
    /// The SCSI medium rotation rate.
    pub medium_rotation_rate: Option<u16>,
    /// The physical sector size.
    pub physical_sector_size: Option<u32>,
    /// Whether FUA is supported.
    pub fua: Option<bool>,
    /// Whether a write cache is present.
    pub write_cache: Option<bool>,
    /// The disk size to present.
    pub scsi_disk_size_in_bytes: Option<u64>,
    /// Whether ODX (copy offload) is supported.
    ///
    /// TODO: remove this, our emulator doesn't support ODX.
    pub odx: Option<bool>,
    /// Whether unmap is supported.
    pub unmap: Option<bool>,
    /// The maximum transfer length for IOs (TODO: or is it for write same?)
    pub max_transfer_length: Option<usize>,
    /// The minimum optimal number of sectors to unmap in a request.
    pub optimal_unmap_sectors: Option<u32>,
}

/// The disk identity.
#[derive(Debug, Clone, Inspect, Protobuf)]
pub struct DiskIdentity {
    /// The vendor ID.
    pub vendor_id: AsciiString<8>,
    /// The product ID.
    pub product_id: AsciiString<16>,
    /// The product revision level.
    pub product_revision_level: AsciiString<4>,
    /// The model number.
    pub model_number: Vec<u8>, // BUGBUG: this is never used.
}

impl DiskIdentity {
    /// Returns the default disk identity, which reports a "Msft Virtual Disk
    /// 1.0".
    pub fn msft() -> Self {
        Self {
            vendor_id: (*b"Msft    ").into(),
            product_id: (*b"Virtual Disk    ").into(),
            product_revision_level: (*b"1.0 ").into(),
            model_number: Vec::new(),
        }
    }
}

/// Resource handle for an emulated SCSI DVD drive.
#[derive(MeshPayload)]
pub struct SimpleScsiDvdHandle {
    /// The backing media, or `None` for an empty DVD drive.
    pub media: Option<Resource<DiskHandleKind>>,
    /// Request channel used to update the contents of the drive.
    pub requests: Option<mesh::Receiver<SimpleScsiDvdRequest>>,
}

/// An emulated DVD drive request.
#[derive(MeshPayload)]
pub enum SimpleScsiDvdRequest {
    /// Change the media to the new backing disk.
    ChangeMedia(FailableRpc<Option<Resource<DiskHandleKind>>, ()>),
}

impl ResourceId<ScsiDeviceHandleKind> for SimpleScsiDvdHandle {
    const ID: &'static str = "emulated_dvd";
}
