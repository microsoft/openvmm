// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Definitions for the mesh entrypoint.
//!
//! These are here instead of in `openvmm_entry` to support launching OpenVMM from
//! a foreign mesh host. The only supported use case is launching OpenVMM from
//! petri for testing.

use mesh::MeshPayload;
use mesh_worker::WorkerHostRunner;

/// The initial message to send when launching a mesh child process.
#[derive(MeshPayload)]
pub struct MeshHostParams {
    pub runner: WorkerHostRunner,
}
