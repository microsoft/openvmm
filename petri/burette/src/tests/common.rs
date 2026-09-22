// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared helpers used by multiple performance tests.

use petri_artifacts_common::tags::MachineArch;

/// Build the default firmware (linux_direct) for the host architecture.
pub fn build_firmware(resolver: &petri::ArtifactResolver<'_>) -> petri::Firmware {
    petri::Firmware::linux_direct(resolver, MachineArch::host())
}

/// Resolve the petritools erofs image for the host architecture.
pub fn require_petritools_erofs(
    resolver: &petri::ArtifactResolver<'_>,
) -> petri_artifacts_core::ResolvedArtifact {
    use petri_artifacts_vmm_test::artifacts::petritools::*;
    match MachineArch::host() {
        MachineArch::X86_64 => resolver.require(PETRITOOLS_EROFS_X64).erase(),
        MachineArch::Aarch64 => resolver.require(PETRITOOLS_EROFS_AARCH64).erase(),
    }
}
