// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resources for VMGS files.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use mesh::MeshPayload;
use vm_resource::kind::NonVolatileStoreKind;
use vm_resource::ResourceId;
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
