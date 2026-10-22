// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Sandbox policy definitions for OpenVMM worker hosts.

use crate::Profile;

/// Selects the sandbox policy for an OpenVMM worker host.
#[derive(Copy, Clone)]
pub enum SandboxRole {
    /// Virtual machine worker host.
    Vm,
    /// TPM device worker host.
    Tpm,
}

impl SandboxRole {
    pub fn name(self) -> &'static str {
        match self {
            Self::Vm => "vm",
            Self::Tpm => "tpm",
        }
    }

    pub fn profile(self) -> Profile {
        match self {
            Self::Vm => super::vm_worker().build(),
            Self::Tpm => super::tpm_worker().build(),
        }
    }
}
