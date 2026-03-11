// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Configuration types for the floppy controller.
//!
//! Resource-based instantiation of floppy controllers is not yet implemented;
//! these types exist in anticipation of that work. The controller is currently
//! instantiated directly as part of the chipset configuration.

#![forbid(unsafe_code)]

use mesh::MeshPayload;
use vm_resource::Resource;
use vm_resource::kind::DiskHandleKind;

/// The configuration for a floppy disk.
#[derive(Debug, MeshPayload)]
pub struct FloppyDiskConfig {
    /// The backing disk media.
    pub disk_type: Resource<DiskHandleKind>,
    /// Whether the disk is read-only.
    pub read_only: bool,
}

/// The configuration for a floppy controller.
#[derive(Debug, MeshPayload)]
pub struct FloppyControllerConfig {
    /// The floppy disks attached to the controller.
    pub floppy_disks: Vec<FloppyDiskConfig>,
}
