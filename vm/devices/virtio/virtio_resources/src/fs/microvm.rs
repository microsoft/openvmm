// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM profile of the virtio-fs resource.
//!
//! [`VirtioFsProfile`] selects the standard virtio-fs device or the microVM
//! device, which serves an attached host folder, an aggregate of host folders
//! backed by [`super::VirtioFsBackend::Aggregate`], or a dormant slot backed
//! by [`super::VirtioFsBackend::Dormant`].

use mesh::MeshPayload;

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
    /// Whether the child exposes the regular file that its root path names,
    /// rather than a directory. A file child has no policy paths.
    pub file: bool,
    pub root_identity: Vec<u8>,
    pub read_only: bool,
    pub denied_paths: Vec<String>,
    pub allowed_paths: Vec<String>,
    pub writable_paths: Vec<String>,
}
