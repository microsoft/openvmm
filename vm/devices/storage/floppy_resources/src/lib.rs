// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Client definitions for describing floppy controller configuration.
//!
//! TODO: refactor to support `Resource`-based instantiation of floppy
//! controllers, at which point this crate name makes sense.

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
