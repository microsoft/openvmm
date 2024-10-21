// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Disk resources, for use with [`vm_resource`].

#![warn(missing_docs)]
#![forbid(unsafe_code)]

use mesh::MeshPayload;
use vm_resource::kind::DiskHandleKind;
use vm_resource::Resource;
use vm_resource::ResourceId;

// Define config types here so that you don't have to pull in the individual
// crates just to describe the configuration.

/// RAM disk handle.
///
/// FUTURE: allocate shared memory here so that the disk can be migrated between
/// processes.
#[derive(MeshPayload)]
pub struct RamDiskHandle {
    /// Size of the disk, in bytes.
    pub len: u64,
}

impl ResourceId<DiskHandleKind> for RamDiskHandle {
    const ID: &'static str = "ram";
}

/// RAM diff disk handle.
#[derive(MeshPayload)]
pub struct RamDiffDiskHandle {
    /// The lower disk resource.
    pub lower: Resource<DiskHandleKind>,
}

impl ResourceId<DiskHandleKind> for RamDiffDiskHandle {
    const ID: &'static str = "ramdiff";
}

/// File-backed disk handle.
#[derive(MeshPayload)]
pub struct FileDiskHandle(pub std::fs::File);

impl ResourceId<DiskHandleKind> for FileDiskHandle {
    const ID: &'static str = "file";
}

/// Disk handle for a disk that emulates persistent reservation support.
#[derive(MeshPayload)]
pub struct DiskWithReservationsHandle(pub Resource<DiskHandleKind>);

impl ResourceId<DiskHandleKind> for DiskWithReservationsHandle {
    const ID: &'static str = "prwrap";
}

/// Disk handle for a fixed VHD1 disk.
#[derive(MeshPayload)]
pub struct FixedVhd1DiskHandle(pub std::fs::File);

impl ResourceId<DiskHandleKind> for FixedVhd1DiskHandle {
    const ID: &'static str = "fixed_vhd1";
}

/// Disk configuration for a striped disk.
#[derive(MeshPayload)]
pub struct StripedDiskHandle {
    /// The underlying disks for the stripes.
    pub devices: Vec<Resource<DiskHandleKind>>,
    /// The size of each stripe.
    pub chunk_size_in_bytes: Option<u32>,
    /// The number of sectors to show for the disk.
    pub logic_sector_count: Option<u64>,
}

impl ResourceId<DiskHandleKind> for StripedDiskHandle {
    const ID: &'static str = "striped";
}

/// Configuration for a disk that is automatically formatted (if it is not
/// already formatted) while being resolved.
// DEVNOTE: this disk type supports a Azure-specific feature in Microsoft's
// closed-source OpenHCL. Due to the NTFS formatting library being used, the
// backing disk type and resolver are currently not able to be open-sourced.
//
// Unfortunately, this feature needs to "leak" into the open-source OpenVMM
// codebase, due to tight coupling in the code of Vtl2Settings.
#[derive(MeshPayload)]
pub struct AutoFormattedDiskHandle {
    /// The disk resource.
    pub disk: Resource<DiskHandleKind>,
    /// The GUID to check for.
    pub guid: [u8; 16],
}

impl ResourceId<DiskHandleKind> for AutoFormattedDiskHandle {
    const ID: &'static str = "ntfsfmt";
}

// blob

/// Handle for a read-only disk backed by a blob served over HTTP.
#[derive(MeshPayload)]
pub struct BlobDiskHandle {
    /// The URL to the disk.
    pub url: String,
    /// The format of the blob.
    pub format: BlobDiskFormat,
}

impl ResourceId<DiskHandleKind> for BlobDiskHandle {
    const ID: &'static str = "blob";
}

/// The format of a disk blob.
#[derive(MeshPayload)]
pub enum BlobDiskFormat {
    /// A flat blob, with no additional metadata.
    Flat,
    /// A fixed VHD1, with a VHD footer specifying disk metadata.
    FixedVhd1,
}
