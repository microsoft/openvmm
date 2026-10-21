// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypervisor resource construction and auto-detection for OpenVMM entry
//! points.

use hypervisor_resources::HypervisorKind;
use hypervisor_resources::MshvConfig;
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

/// Parsed hypervisor selection, before any backend device is opened.
pub struct HypervisorParamRoot<'a> {
    /// Backend name.
    pub name: &'a str,
    /// Backend-specific key-value pairs.
    pub params: Vec<(&'a str, &'a str)>,
}

impl<'a> HypervisorParamRoot<'a> {
    /// Parses `name` or `name:key=val,key,...`.
    ///
    /// A bare parameter has the value `"true"`.
    pub fn parse(spec: &'a str) -> anyhow::Result<Self> {
        let (name, rest) = spec.split_once(':').unwrap_or((spec, ""));
        anyhow::ensure!(!name.is_empty(), "empty hypervisor name in spec: {spec}");
        let params = if rest.is_empty() {
            Vec::new()
        } else {
            rest.split(',')
                .filter(|item| !item.is_empty())
                .map(|item| {
                    let (key, val) = item.split_once('=').unwrap_or((item, "true"));
                    anyhow::ensure!(!key.is_empty(), "empty parameter key in spec: {spec}");
                    Ok((key, val))
                })
                .collect::<anyhow::Result<Vec<_>>>()?
        };
        if name == "mshv" {
            MshvConfig::parse(&params)?;
        }
        Ok(Self { name, params })
    }
}

/// Whether the selected backend and isolation require owned device IO.
///
/// Call after backend selection, including auto-detection.
pub fn requires_snp_bounce(backend: &str, snp: bool) -> bool {
    backend == "mshv" && snp
}

/// Returns a [`Resource<HypervisorKind>`] for the named backend, with
/// optional parameters.
///
/// The specifier format is `name` or `name:key=val,key,...`.
/// Each backend validates its own parameters — see the probe
/// implementations for supported keys.
pub fn hypervisor_resource(spec: &str) -> anyhow::Result<Resource<HypervisorKind>> {
    let parsed = HypervisorParamRoot::parse(spec)?;
    let probe = hypervisor_resources::probe_by_name(parsed.name)
        .ok_or_else(|| anyhow::anyhow!("unknown hypervisor: {}", parsed.name))?;
    probe.new_resource(&parsed.params)
}
