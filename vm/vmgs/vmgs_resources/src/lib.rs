// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resources for VMGS files.

#![forbid(unsafe_code)]

use mesh::MeshPayload;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::DiskHandleKind;
use vm_resource::kind::NonVolatileStoreKind;
use vmgs_format::FileId;

/// A handle to an individual file within a VMGS file.
#[derive(MeshPayload)]
pub struct VmgsFileHandle {
    /// The file ID.
    ///
    /// FUTURE: figure out how to give this the nice type.
    pub file_id: u32,
    /// Whether the file is encrypted.
    pub encrypted: bool,
}

impl VmgsFileHandle {
    /// Returns a new handle to the given file.
    pub fn new(file_id: FileId, encrypted: bool) -> Self {
        Self {
            file_id: file_id.0,
            encrypted,
        }
    }
}

impl ResourceId<NonVolatileStoreKind> for VmgsFileHandle {
    const ID: &'static str = "vmgs";
}

/// Virtual machine guest state resource
#[derive(MeshPayload, Debug)]
pub enum VmgsResource {
    /// Use disk to store guest state
    Disk(Resource<DiskHandleKind>),
    /// Use disk to store guest state, reformatting if corrupted.
    ReprovisionOnFailure(Resource<DiskHandleKind>),
    /// Format and use disk to store guest state
    Reprovision(Resource<DiskHandleKind>),
    /// Store guest state in memory
    Ephemeral,
}
