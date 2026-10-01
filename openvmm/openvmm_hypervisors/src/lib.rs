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
    use test_with_tracing::test;

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
