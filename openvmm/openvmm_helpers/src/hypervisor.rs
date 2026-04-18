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

/// Parses a hypervisor specifier of the form `name` or `name:key=val,key,...`.
///
/// Returns `(name, params)` where `params` is a list of `(key, value)` pairs.
/// A bare key (no `=`) is treated as a boolean flag with value `"true"`.
fn parse_hypervisor_spec(spec: &str) -> (&str, Vec<(&str, &str)>) {
    let (name, rest) = spec.split_once(':').unwrap_or((spec, ""));
    let params = if rest.is_empty() {
        Vec::new()
    } else {
        rest.split(',')
            .map(|item| item.split_once('=').unwrap_or((item, "true")))
            .collect()
    };
    (name, params)
}

/// Returns a [`Resource<HypervisorKind>`] for the named backend, with
/// optional parameters.
///
/// The specifier format is `name` or `name:key=val,key,...`.
/// Each backend validates its own parameters — see the probe
/// implementations for supported keys.
pub fn hypervisor_resource(spec: &str) -> anyhow::Result<Resource<HypervisorKind>> {
    let (name, params) = parse_hypervisor_spec(spec);
    let probe = hypervisor_resources::probe_by_name(name)
        .ok_or_else(|| anyhow::anyhow!("unknown hypervisor: {name}"))?;
    probe.new_resource(&params)
}
