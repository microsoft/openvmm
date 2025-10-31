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
    Disk(VmgsDisk),
    /// Use disk to store guest state, reformatting if corrupted.
    ReprovisionOnFailure(VmgsDisk),
    /// Format and use disk to store guest state
    Reprovision(VmgsDisk),
    /// Store guest state in memory
    Ephemeral,
}

impl VmgsResource {
    /// get the encryption policy (returns None for ephemeral guest state)
    pub fn encryption_policy(&self) -> GuestStateEncryptionPolicy {
        match self {
            VmgsResource::Disk(vmgs)
            | VmgsResource::ReprovisionOnFailure(vmgs)
            | VmgsResource::Reprovision(vmgs) => vmgs.encryption_policy,
            VmgsResource::Ephemeral => GuestStateEncryptionPolicy::None(true),
        }
    }
}

/// VMGS disk resource
#[derive(MeshPayload, Debug)]
pub struct VmgsDisk {
    /// Backing disk
    pub disk: Resource<DiskHandleKind>,
    /// Guest state encryption policy
    pub encryption_policy: GuestStateEncryptionPolicy,
}

/// Guest state encryption policy
///
/// See detailed comments in `get_protocol`
#[derive(MeshPayload, Debug, Clone, Copy)]
pub enum GuestStateEncryptionPolicy {
    /// Use the best encryption available, allowing fallback.
    Auto,
    /// Prefer (or require, if strict) no encryption.
    None(bool),
    /// Prefer (or require, if strict) GspById.
    GspById(bool),
    /// Prefer (or require, if strict) GspKey.
    GspKey(bool),
}

impl GuestStateEncryptionPolicy {
    /// whether to use strict encryption policy
    pub fn is_strict(&self) -> bool {
        match self {
            GuestStateEncryptionPolicy::Auto => false,
            GuestStateEncryptionPolicy::None(strict)
            | GuestStateEncryptionPolicy::GspById(strict)
            | GuestStateEncryptionPolicy::GspKey(strict) => *strict,
        }
    }
}
