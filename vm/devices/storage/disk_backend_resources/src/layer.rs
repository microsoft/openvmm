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

/// Parameters used when performing first-time init of `dbhd` files.
#[derive(MeshPayload)]
pub struct SqliteDiskLayerFormatParams {
    /// Should the layer be considered logically read only (i.e: a cache layer)
    pub logically_read_only: bool,
    /// Desired layer size. If `None`, lazily selects a size only once after
    /// being attached to an existing layer.
    pub len: Option<u64>,
}

/// Sqlite disk layer handle.
#[derive(MeshPayload)]
pub struct SqliteDiskLayerHandle {
    /// Path to `.dbhd` file
    pub dbhd_path: String,

    /// If this is provided, the dbhd will be (re)formatted with the provided
    /// params.
    pub format_dbhd: Option<SqliteDiskLayerFormatParams>,
}

impl ResourceId<DiskLayerHandleKind> for SqliteDiskLayerHandle {
    const ID: &'static str = "sqlite";
}

/// A handle for a disk layer that automatically selects a dbhd file to use as a
/// cache for lower layers.
#[derive(MeshPayload)]
pub struct SqliteAutoCacheDiskLayerHandle {
    /// Path to the root directory for the cache.
    pub cache_path: String,
    /// The key to use to select the cache file. If `None`, use the next layer's
    /// disk ID.
    pub cache_key: Option<String>,
}

impl ResourceId<DiskLayerHandleKind> for SqliteAutoCacheDiskLayerHandle {
    const ID: &'static str = "sqlite-autocache";
}
