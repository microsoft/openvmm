// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Disk layer resources.

use mesh::MeshPayload;
use vm_resource::kind::DiskHandleKind;
use vm_resource::kind::DiskLayerHandleKind;
use vm_resource::Resource;
use vm_resource::ResourceId;

/// RAM disk layer handle.
///
/// FUTURE: allocate shared memory here so that the disk can be migrated between
/// processes.
#[derive(MeshPayload)]
pub struct RamDiskLayerHandle {
    /// The size of the layer. If `None`, the layer will be the same size as the
    /// lower disk.
    pub len: Option<u64>,
}

impl ResourceId<DiskLayerHandleKind> for RamDiskLayerHandle {
    const ID: &'static str = "ram";
}

/// Handle for a disk layer backed by a full disk.
#[derive(MeshPayload)]
pub struct DiskLayerHandle(pub Resource<DiskHandleKind>);

impl ResourceId<DiskLayerHandleKind> for DiskLayerHandle {
    const ID: &'static str = "disk";
}
