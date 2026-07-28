// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Measured product policy integration: decode from the measured VTL2
//! config region and post-load validation. Only compiled when the
//! `product_policy` feature is enabled.

use crate::dispatch::LoadedVm;
use anyhow::Context as _;
use firmware_uefi_custom_vars::CustomVars;
use get_protocol::dps_json::GuestStateLifetime;
use product_policy::MeasuredProductPolicy;
use product_policy::UefiSecurityPolicy;

/// Decode and integrity-check a product policy body read from the measured
/// VTL2 config region. `size` is the declared `product_policy_size`.
pub fn decode(buf: &[u8], size: usize) -> anyhow::Result<MeasuredProductPolicy> {
    let policy = product_policy::decode_product_policy(buf)
        .map_err(anyhow::Error::from)
        .context("product policy decode failed")?;

    // Integrity check to ensure we are enforcing the complete policy
    let encoded_len = product_policy::encode_product_policy(&policy).len();
    if encoded_len != size {
        anyhow::bail!(
            "product policy size mismatch: declared {size} bytes, re-encoded {encoded_len} bytes"
        );
    }
    Ok(MeasuredProductPolicy::new(Some(policy)))
}

fn validate_uefi_security_policy(
    policy: &dyn UefiSecurityPolicy,
    vm: &LoadedVm,
) -> anyhow::Result<()> {
    policy.validate_secure_boot_enabled(vm.device_platform_settings.general.secure_boot_enabled)?;
    policy.validate_secure_boot_policy_enforcement()?;
    Ok(())
}

/// Enforce the policy's ephemeral-VMGS requirement, called before the VMGS is
/// opened so a policy requiring ephemeral can't trigger a host-VMGS read.
pub fn enforce_ephemeral_vmgs(
    policy: &MeasuredProductPolicy,
    guest_state_lifetime: GuestStateLifetime,
) -> anyhow::Result<()> {
    let vmgs_is_ephemeral = matches!(guest_state_lifetime, GuestStateLifetime::Ephemeral);
    policy.sivm(|p| p.enforce_ephemeral_vmgs_required(vmgs_is_ephemeral))?;
    policy.cwcow(|p| p.enforce_ephemeral_vmgs_required(vmgs_is_ephemeral))?;
    Ok(())
}

fn validated_uefi_json_fn<T>(p: &T) -> anyhow::Result<Vec<u8>>
where
    T: UefiSecurityPolicy,
{
    Ok(p.get_validated_uefi_json()?.to_vec())
}

/// Return the measured, validated UEFI nvram state from the policy applied onto
/// `base_vars`, or `None` when the policy is not present.
pub fn measured_uefi_nvram_state(
    policy: &MeasuredProductPolicy,
    base_vars: &CustomVars,
) -> anyhow::Result<Option<CustomVars>> {
    let uefi_state_json = policy
        .sivm(validated_uefi_json_fn)?
        .or(policy.cwcow(validated_uefi_json_fn)?);
    if let Some(uefi_state) = uefi_state_json {
        let delta = hyperv_uefi_custom_vars_json::load_delta_from_json(&uefi_state)?;
        let measured = base_vars.clone().apply_delta(delta)?;
        return Ok(Some(measured));
    }
    Ok(None)
}

/// Post-load validation of the measured product policy.
pub fn validate(loaded_vm: &LoadedVm) -> anyhow::Result<()> {
    loaded_vm
        .measured_product_policy
        .sivm(|p| validate_uefi_security_policy(p, loaded_vm))?;
    loaded_vm
        .measured_product_policy
        .cwcow(|p| validate_uefi_security_policy(p, loaded_vm))?;

    #[cfg(guest_arch = "x86_64")]
    {
        let hardware_secure_avic_enabled = loaded_vm.partition.secure_avic_enabled();
        loaded_vm
            .measured_product_policy
            .cwcow(|p| p.enforce_secure_avic(hardware_secure_avic_enabled))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use product_policy::ProductPolicy;
    use product_policy::sivm::SivmPolicy;
    use test_with_tracing::test;

    use super::decode;

    #[test]
    fn decode_checks_measured_policy_size() {
        let expected = ProductPolicy::Sivm(SivmPolicy {
            require_ephemeral_vmgs: true,
            require_secure_boot: true,
            ..Default::default()
        });
        let encoded = product_policy::encode_product_policy(&expected);

        let measured = decode(&encoded, encoded.len()).expect("policy should decode");
        assert_eq!(measured.raw(), Some(&expected));

        let error = decode(&encoded, encoded.len() + 1)
            .expect_err("a declared size mismatch should be rejected");
        assert!(error.to_string().contains("product policy size mismatch"));
    }
}
