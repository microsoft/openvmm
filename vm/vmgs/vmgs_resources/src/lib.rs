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
#[derive(MeshPayload, Debug, Clone, Copy)]
pub enum GuestStateEncryptionPolicy {
    /// Use the best encryption available, allowing fallback.
    ///
    /// VMs will be created as or migrated to the best encryption available,
    /// attempting GspKey, then GspById, and finally leaving the data
    /// unencrypted if neither are available.
    Auto,
    /// Prefer (or require, if strict) no encryption.
    ///
    /// Do not encrypt the guest state unless it is already encrypted and
    /// strict encryption policy is disabled.
    None(bool),
    /// Prefer (or require, if strict) GspById.
    ///
    /// This prevents a VM from being created as or migrated to GspKey even
    /// if it is available. Exisiting GspKey encryption will be used unless
    /// strict encryption policy is enabled. Fails if the data cannot be
    /// encrypted.
    GspById(bool),
    /// Require GspKey.
    ///
    /// VMs will be created as or migrated to GspKey. Fails if GspKey is
    /// not available. Strict encryption policy has no effect here since
    /// GspKey is currently the most secure policy.
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
