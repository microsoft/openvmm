// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM profile of the virtio-fs resource.
//!
//! [`VirtioFsProfile`] selects the standard virtio-fs device or the microVM
//! device, which serves either an attached host folder or a dormant slot
//! backed by [`super::VirtioFsBackend::Dormant`].

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
    MicrovmDormant {
        stable_id: String,
    },
}
