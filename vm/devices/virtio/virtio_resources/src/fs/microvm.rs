// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM profile of the virtio-fs resource.
//!
//! [`VirtioFsProfile`] selects the standard virtio-fs device or the microVM
//! device, which serves an attached host folder, an aggregate of host folders
//! backed by [`super::VirtioFsBackend::Aggregate`], or a dormant slot backed
//! by [`super::VirtioFsBackend::Dormant`].

use mesh::MeshPayload;
use mesh::payload::Protobuf;

/// Access to an explicitly mapped subtree. Ordering keeps the most restrictive
/// grant when several rules name the same path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Protobuf)]
#[mesh(package = "virtio.fs")]
pub enum MicrovmPathAccess {
    /// Directory names and ordinary attributes, without file-content access.
    #[mesh(1)]
    Enumerate,
    /// Content access without modification.
    #[mesh(2)]
    ReadOnly,
    /// Content access and modification.
    #[mesh(3)]
    ReadWrite,
}

/// A canonical share-relative path and the access granted to its subtree.
#[derive(Debug, Clone, PartialEq, Eq, Protobuf)]
#[mesh(package = "virtio.fs")]
pub struct MicrovmPathAccessRule {
    /// Canonical share-relative path; empty names the root.
    #[mesh(1)]
    pub path: String,
    /// Access to this path and descendants without a more specific rule.
    #[mesh(2)]
    pub access: MicrovmPathAccess,
}

#[derive(MeshPayload)]
pub enum VirtioFsProfile {
    Standard,
    Microvm {
        stable_id: String,
        root_identity: Vec<u8>,
        read_only: bool,
        /// Canonical share-relative paths hidden from the guest.
        denied_paths: Vec<String>,
        /// Canonical share-relative paths inside `denied_paths` that the guest
        /// can reach again.
        allowed_paths: Vec<String>,
        /// Canonical share-relative paths that are the only parts of a
        /// read-write share that the guest can modify; none makes the whole
        /// share writable.
        writable_paths: Vec<String>,
        /// Perform each guest request as the host UID and GID of its caller,
        /// with root squashed to the owner of the export root, instead of as
        /// the VMM. Linux only.
        caller_identity: bool,
        /// Explicit subtree access. Empty preserves the legacy write policy.
        access_rules: Vec<MicrovmPathAccessRule>,
    },
    /// An aggregate whose synthetic, read-only root lists one directory or
    /// regular file per child. The children of the `Aggregate` backend must
    /// have the same names, in the same order.
    MicrovmAggregate {
        stable_id: String,
        children: Vec<MicrovmAggregateChild>,
        /// Perform each guest request as its caller, like
        /// [`VirtioFsProfile::Microvm`]'s `caller_identity`. Every child root
        /// must then have the same owner.
        caller_identity: bool,
    },
    MicrovmDormant {
        stable_id: String,
    },
}

/// The identity and access policy of one child of a
/// [`VirtioFsProfile::MicrovmAggregate`], with the meaning of the
/// corresponding fields of [`VirtioFsProfile::Microvm`].
#[derive(MeshPayload)]
pub struct MicrovmAggregateChild {
    /// Name of the child under the synthetic root.
    pub name: String,
    /// The kind of host object that the child's root path names.
    pub kind: MicrovmAggregateChildKind,
    pub root_identity: Vec<u8>,
    pub read_only: bool,
    pub denied_paths: Vec<String>,
    pub allowed_paths: Vec<String>,
    pub writable_paths: Vec<String>,
    pub access_rules: Vec<MicrovmPathAccessRule>,
}

/// The kind of host object that a [`MicrovmAggregateChild`] exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, MeshPayload)]
pub enum MicrovmAggregateChildKind {
    /// A host directory, with its own denied, allowed, and writable paths.
    Directory,
    /// A regular host file, without its directory. Its access mode applies to
    /// the whole file, so it takes no denied, allowed, or writable paths.
    File,
}
