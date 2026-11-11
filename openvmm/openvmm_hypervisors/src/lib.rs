// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypervisor backend implementations for OpenVMM.
//!
//! Each submodule provides a [`HypervisorProbe`](hypervisor_resources::HypervisorProbe)
//! implementation for the corresponding hypervisor backend.
//!
//! Probes are registered here via `register_hypervisor_probes!`. Resource
//! resolvers are registered separately in `openvmm_resources`.

#![forbid(unsafe_code)]

pub mod hvf;
pub mod kvm;
pub mod mshv;
pub mod whp;

// Register probes for auto-detection (checked in this order).
hypervisor_resources::register_hypervisor_probes! {
    #[cfg(all(target_os = "linux", feature = "virt_mshv", guest_is_native))]
    mshv::MshvProbe,

    #[cfg(all(target_os = "linux", feature = "virt_kvm", guest_is_native))]
    kvm::KvmProbe,

    #[cfg(all(target_os = "windows", feature = "virt_whp", guest_is_native))]
    whp::WhpProbe,

    #[cfg(all(target_os = "macos", guest_arch = "aarch64", guest_is_native, feature = "virt_hvf"))]
    hvf::HvfProbe,
}

/// Returns whether OpenVMM can create SEV-SNP partitions with MSHV on this
/// machine. See `virt_mshv::is_snp_available` for why this is a probe.
pub fn mshv_snp_available() -> bool {
    #[cfg(all(
        target_os = "linux",
        feature = "virt_mshv",
        guest_is_native,
        guest_arch = "x86_64"
    ))]
    {
        virt_mshv::is_snp_available()
    }
    #[cfg(not(all(
        target_os = "linux",
        feature = "virt_mshv",
        guest_is_native,
        guest_arch = "x86_64"
    )))]
    {
        false
    }
}

#[expect(clippy::allow_attributes, reason = "lots of conditions")]
#[allow(dead_code)]
pub(crate) fn parse_bool_param(key: &str, val: &str) -> anyhow::Result<bool> {
    match val {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => anyhow::bail!("invalid boolean value for {key}: {val}"),
    }
}

/// Rejects backend parameters for CPU fingerprinting, which none of them
/// affect.
#[expect(clippy::allow_attributes, reason = "lots of conditions")]
#[allow(dead_code)]
pub(crate) fn reject_fingerprint_params(
    backend: &str,
    params: &[(&str, &str)],
) -> anyhow::Result<()> {
    if let Some(&(key, _)) = params.first() {
        anyhow::bail!("{backend} parameter {key} does not apply to CPU fingerprinting");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::reject_fingerprint_params;
    use cpu_profile::fingerprint::BackendCpuFingerprint;
    use cpu_profile::fingerprint::BackendFingerprint;
    use hypervisor_resources::HypervisorKind;
    use hypervisor_resources::HypervisorProbe;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;
    use vm_resource::Resource;

    struct LegacyProvider;

    impl HypervisorProbe for LegacyProvider {
        fn name(&self) -> &str {
            "legacy-test"
        }

        fn try_new_resource(&self) -> anyhow::Result<Option<Resource<HypervisorKind>>> {
            anyhow::bail!("resource construction is not part of this test")
        }

        fn new_resource(&self, _: &[(&str, &str)]) -> anyhow::Result<Resource<HypervisorKind>> {
            anyhow::bail!("resource construction is not part of this test")
        }

        fn cpu_fingerprint(&self, params: &[(&str, &str)]) -> anyhow::Result<BackendFingerprint> {
            reject_fingerprint_params(self.name(), params)?;
            Ok(BackendFingerprint::new(self.name(), "legacy", vec![]))
        }
    }

    struct ArmProvider(AtomicUsize);

    impl HypervisorProbe for ArmProvider {
        fn name(&self) -> &str {
            "arm-test"
        }

        fn try_new_resource(&self) -> anyhow::Result<Option<Resource<HypervisorKind>>> {
            anyhow::bail!("resource construction is not part of this test")
        }

        fn new_resource(&self, _: &[(&str, &str)]) -> anyhow::Result<Resource<HypervisorKind>> {
            anyhow::bail!("resource construction is not part of this test")
        }

        fn probe_cpu(&self, params: &[(&str, &str)]) -> anyhow::Result<BackendCpuFingerprint> {
            reject_fingerprint_params(self.name(), params)?;
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(cpu_profile::aarch64::Fingerprint {
                midr: 0x410f_d030,
                registers: vec![],
                counter_hz: 24_000_000,
                backend_usable_gpa_bits: 39,
            }
            .into())
        }
    }

    #[test]
    fn legacy_provider_uses_the_default_typed_bridge_without_api_changes() {
        let provider: &dyn HypervisorProbe = &LegacyProvider;
        let legacy = provider.cpu_fingerprint(&[]).unwrap();
        assert_eq!(provider.probe_cpu(&[]).unwrap().into_x86().unwrap(), legacy);
        assert!(provider.probe_cpu(&[]).unwrap().into_aarch64().is_err());
        let params = &[("invalid", "true")];
        assert_eq!(
            provider.probe_cpu(params).unwrap_err().to_string(),
            provider.cpu_fingerprint(params).unwrap_err().to_string()
        );
    }

    #[test]
    fn typed_arm_probe_does_not_change_legacy_unsupported_behavior() {
        let arm = ArmProvider(AtomicUsize::new(0));
        let provider: &dyn HypervisorProbe = &arm;
        let error = provider.cpu_fingerprint(&[]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the arm-test hypervisor backend does not support CPU fingerprinting"
        );
        assert_eq!(arm.0.load(Ordering::SeqCst), 0);
        let native = provider.probe_cpu(&[]).unwrap().into_aarch64().unwrap();
        assert_eq!(native.midr, 0x410f_d030);
        assert_eq!(native.counter_hz, 24_000_000);
        assert_eq!(native.backend_usable_gpa_bits, 39);
        assert_eq!(arm.0.load(Ordering::SeqCst), 1);
        assert!(provider.probe_cpu(&[]).unwrap().into_x86().is_err());
        assert!(provider.probe_cpu(&[("invalid", "true")]).is_err());
        assert_eq!(arm.0.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn fingerprinting_rejects_backend_parameters() {
        reject_fingerprint_params("whp", &[]).unwrap();
        let error = reject_fingerprint_params("whp", &[("user_mode_apic", "true")]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "whp parameter user_mode_apic does not apply to CPU fingerprinting"
        );
    }
}
