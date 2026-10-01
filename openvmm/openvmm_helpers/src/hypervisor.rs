// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypervisor resource construction and auto-detection for OpenVMM entry
//! points.

use cpu_profile::fingerprint::BackendFingerprint;
use hypervisor_resources::HypervisorKind;
use vm_resource::Resource;

pub mod microvm;

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
fn parse_hypervisor_spec(spec: &str) -> anyhow::Result<(&str, Vec<(&str, &str)>)> {
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
    Ok((name, params))
}

/// Returns a [`Resource<HypervisorKind>`] for the named backend, with
/// optional parameters.
///
/// The specifier format is `name` or `name:key=val,key,...`.
/// Each backend validates its own parameters — see the probe
/// implementations for supported keys.
pub fn hypervisor_resource(spec: &str) -> anyhow::Result<Resource<HypervisorKind>> {
    let (name, params) = parse_hypervisor_spec(spec)?;
    let probe = hypervisor_resources::probe_by_name(name)
        .ok_or_else(|| anyhow::anyhow!("unknown hypervisor: {name}"))?;
    probe.new_resource(&params)
}

/// Returns the guest CPU surface that a hypervisor backend supports on this
/// host, for a host CPU fingerprint.
///
/// `spec` selects the backend as for [`hypervisor_resource`]. Without it, the
/// first available backend is used, as for [`choose_hypervisor`].
pub fn cpu_fingerprint(spec: Option<&str>) -> anyhow::Result<BackendFingerprint> {
    match spec {
        Some(spec) => {
            let (name, params) = parse_hypervisor_spec(spec)?;
            let probe = hypervisor_resources::probe_by_name(name)
                .ok_or_else(|| anyhow::anyhow!("unknown hypervisor: {name}"))?;
            probe.cpu_fingerprint(&params)
        }
        None => {
            for probe in hypervisor_resources::probes() {
                if probe.try_new_resource()?.is_some() {
                    return probe.cpu_fingerprint(&[]);
                }
            }
            anyhow::bail!("no hypervisor available");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::cpu_fingerprint;

    #[test]
    fn fingerprint_rejects_unknown_hypervisor() {
        let error = cpu_fingerprint(Some("nosuch")).unwrap_err();
        assert_eq!(error.to_string(), "unknown hypervisor: nosuch");
        let error = cpu_fingerprint(Some(":x")).unwrap_err();
        assert_eq!(error.to_string(), "empty hypervisor name in spec: :x");
    }
}
