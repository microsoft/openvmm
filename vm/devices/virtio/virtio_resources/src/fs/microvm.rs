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
        denied_paths: Vec<String>,
    },
    MicrovmDormant {
        stable_id: String,
    },
}
