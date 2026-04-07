// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypervisor resource construction and auto-detection for OpenVMM entry
//! points.

use hypervisor_resources::HypervisorKind;
use vm_resource::Resource;

/// Returns a [`Resource<HypervisorKind>`] for the first available hypervisor
/// backend.
///
/// Backends are checked in registration order (highest priority first).
pub fn choose_hypervisor() -> anyhow::Result<Resource<HypervisorKind>> {
    for probe in hypervisor_resources::probes() {
        if let Some(resource) = probe.try_new_resource()? {
            return Ok(resource);
        }
    }
    anyhow::bail!("no hypervisor available");
}

/// Returns a [`Resource<HypervisorKind>`] for the named backend.
///
/// This validates that the name matches a registered probe and checks
/// availability.
pub fn hypervisor_resource(name: &str) -> anyhow::Result<Resource<HypervisorKind>> {
    let probe = hypervisor_resources::probe_by_name(name)
        .ok_or_else(|| anyhow::anyhow!("unknown hypervisor: {name}"))?;
    probe
        .try_new_resource()?
        .ok_or_else(|| anyhow::anyhow!("hypervisor {name} is not available"))
}
