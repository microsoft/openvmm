// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM machine profile definitions.

use mesh::MeshPayload;

/// The guest-visible machine contract, independent of the hypervisor backend.
#[derive(MeshPayload, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum MachineProfile {
    /// The standard OpenVMM machine.
    #[default]
    Standard,
    /// The microVM machine.
    Microvm,
}

/// VM configuration specific to the microVM machine profile
/// ([`MachineProfile::Microvm`]), held in
/// [`Config::microvm`](crate::config::Config::microvm).
#[derive(MeshPayload, Debug, Default)]
pub struct MicrovmConfig {}
