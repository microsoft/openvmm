// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypervisor resource construction and auto-detection for OpenVMM entry
//! points.

use cpu_profile::CpuProfile;
use cpu_profile::cpuid::CpuidEntry;
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
    fingerprint_backend(spec)?.cpu_fingerprint()
}

/// A hypervisor backend selected for a host CPU fingerprint and its checks.
pub struct FingerprintBackend<'a> {
    probe: &'static dyn hypervisor_resources::HypervisorProbe,
    params: Vec<(&'a str, &'a str)>,
}

/// Selects the hypervisor backend for a host CPU fingerprint.
///
/// `spec` selects the backend as for [`hypervisor_resource`]. Without it, the
/// first available backend is used, as for [`choose_hypervisor`].
pub fn fingerprint_backend(spec: Option<&str>) -> anyhow::Result<FingerprintBackend<'_>> {
    match spec {
        Some(spec) => {
            let (name, params) = parse_hypervisor_spec(spec)?;
            let probe = hypervisor_resources::probe_by_name(name)
                .ok_or_else(|| anyhow::anyhow!("unknown hypervisor: {name}"))?;
            Ok(FingerprintBackend { probe, params })
        }
        None => {
            for probe in hypervisor_resources::probes() {
                if probe.try_new_resource()?.is_some() {
                    return Ok(FingerprintBackend {
                        probe,
                        params: Vec::new(),
                    });
                }
            }
            anyhow::bail!("no hypervisor available");
        }
    }
}

impl FingerprintBackend<'_> {
    /// Returns the guest CPU surface that the backend supports on this host.
    pub fn cpu_fingerprint(&self) -> anyhow::Result<BackendFingerprint> {
        self.probe.cpu_fingerprint(&self.params)
    }

    /// Returns what VP 0 of a probe partition configured from `profile`
    /// reads at the host's CPUID entries outside the profile's tables, or
    /// `None` if the backend has no such probe; see
    /// [`HypervisorProbe::profile_unlisted_cpuid`](hypervisor_resources::HypervisorProbe::profile_unlisted_cpuid).
    pub fn profile_unlisted_cpuid(
        &self,
        profile: &CpuProfile,
    ) -> anyhow::Result<Option<Vec<CpuidEntry>>> {
        self.probe.profile_unlisted_cpuid(&self.params, profile)
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
