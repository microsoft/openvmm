// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! WHP hypervisor backend.

#![cfg(all(target_os = "windows", feature = "virt_whp", guest_is_native))]

use crate::parse_bool_param;
use hypervisor_resources::HypervisorKind;
use hypervisor_resources::WhpHandle;
use vm_resource::Resource;

/// WHP probe for auto-detection.
pub struct WhpProbe;

impl hypervisor_resources::HypervisorProbe for WhpProbe {
    fn name(&self) -> &str {
        "whp"
    }

    fn try_new_resource(&self) -> anyhow::Result<Option<Resource<HypervisorKind>>> {
        Ok(virt_whp::is_available()?.then(|| Resource::new(WhpHandle::default())))
    }

    fn new_resource(&self, params: &[(&str, &str)]) -> anyhow::Result<Resource<HypervisorKind>> {
        let mut handle = WhpHandle::default();
        for &(key, val) in params {
            match key {
                "user_mode_apic" => {
                    if cfg!(guest_arch = "x86_64") {
                        handle.user_mode_apic = parse_bool_param(key, val)?;
                    } else {
                        anyhow::bail!("whp parameter {key} is only supported for x86_64 guests");
                    }
                }
                "no_enlightenments" => {
                    if cfg!(guest_arch = "x86_64") {
                        handle.offload_enlightenments = !parse_bool_param(key, val)?;
                    } else {
                        anyhow::bail!("whp parameter {key} is only supported for x86_64 guests");
                    }
                }
                _ => anyhow::bail!("unknown whp parameter: {key}"),
            }
        }
        anyhow::ensure!(virt_whp::is_available()?, "WHP is not available");
        Ok(Resource::new(handle))
    }

    #[cfg(guest_arch = "x86_64")]
    fn cpu_fingerprint(
        &self,
        params: &[(&str, &str)],
    ) -> anyhow::Result<cpu_profile::fingerprint::BackendFingerprint> {
        crate::reject_fingerprint_params("whp", params)?;
        anyhow::ensure!(virt_whp::is_available()?, "WHP is not available");
        Ok(virt_whp::cpu_fingerprint()?)
    }

    #[cfg(guest_arch = "aarch64")]
    fn probe_cpu(
        &self,
        params: &[(&str, &str)],
    ) -> anyhow::Result<cpu_profile::fingerprint::BackendCpuFingerprint> {
        crate::reject_fingerprint_params("whp", params)?;
        anyhow::ensure!(virt_whp::is_available()?, "WHP is not available");
        Ok(virt_whp::cpu_fingerprint()?.into())
    }

    #[cfg(guest_arch = "x86_64")]
    fn profile_unlisted_cpuid(
        &self,
        params: &[(&str, &str)],
        profile: &cpu_profile::CpuProfile,
    ) -> anyhow::Result<Option<Vec<cpu_profile::cpuid::CpuidEntry>>> {
        crate::reject_fingerprint_params("whp", params)?;
        anyhow::ensure!(virt_whp::is_available()?, "WHP is not available");
        Ok(Some(virt_whp::profile_unlisted_cpuid(profile)?))
    }
}

#[cfg(all(test, guest_arch = "aarch64"))]
mod tests {
    use super::*;
    use hypervisor_resources::HypervisorProbe;
    use test_with_tracing::test;

    #[test]
    fn arm_legacy_fingerprint_is_unsupported_without_native_probe() {
        // The default legacy method must fail even with invalid probe parameters,
        // before any WHP availability query or transient partition creation.
        let error = WhpProbe
            .cpu_fingerprint(&[("invalid", "true")])
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "the whp hypervisor backend does not support CPU fingerprinting"
        );
    }

    #[test]
    #[ignore = "requires native ARM64 Windows WHP; no guest entry"]
    fn native_typed_arm_probe_records_backend_limit_separately_from_profile_width() {
        let native = WhpProbe.probe_cpu(&[]).unwrap().into_aarch64().unwrap();
        let profile = cpu_profile::aarch64::select(cpu_profile::AUTO, &native).unwrap();
        assert!(native.backend_usable_gpa_bits > 0);
        assert!(native.counter_hz > 0);
        assert_ne!(native.midr, 0);
        assert_eq!(profile.physical_address_bits, 36);
        println!(
            "ARM_TYPED_PROBE backend_usable_gpa_bits={} profile_architectural_pa_bits={} native_f={}",
            native.backend_usable_gpa_bits, profile.physical_address_bits, native.counter_hz,
        );
    }
}
