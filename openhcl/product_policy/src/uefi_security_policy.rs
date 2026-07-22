// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared validation for the currently enforced UEFI product-policy
//! checks (secure-boot-required), used by product policy variants
//! (Sivm, Cwcow, etc.).

/// Internal trait providing access to policy fields needed by the
/// shared validation logic. Kept crate-private so raw getters are not
/// exposed outside the crate.
pub(crate) trait UefiSecurityPolicyParams {
    fn require_secure_boot(&self) -> bool;
    fn require_secure_boot_vars(&self) -> bool;
    fn require_bcd_integrity(&self) -> bool;
    fn custom_uefi_json(&self) -> &[u8];
    fn require_ephemeral_vmgs(&self) -> bool;
}

/// A trait for validating UEFI security settings. Implementors only
/// need to provide `UefiSecurityPolicyParams`; all methods here have
/// default bodies, so policies can use an empty marker impl.
#[expect(
    private_bounds,
    reason = "Params getters are intentionally crate-private; only default methods are public"
)]
pub trait UefiSecurityPolicy
where
    Self: UefiSecurityPolicyParams,
{
    /// Validate that secure boot is enabled if required by the policy.
    fn validate_secure_boot_enabled(&self, on: bool) -> anyhow::Result<()> {
        if self.require_secure_boot() && !on {
            anyhow::bail!("product policy requires secure boot to be enabled");
        }
        Ok(())
    }

    /// Validate the secure boot policy enforcement from the custom UEFI JSON.
    fn validate_secure_boot_policy_enforcement(&self) -> anyhow::Result<()> {
        validate_secure_boot_policy_enforcement_common(self)
    }

    /// Return the custom UEFI JSON after validating it, or an error.
    fn get_validated_uefi_json(&self) -> anyhow::Result<&[u8]> {
        if self.custom_uefi_json().is_empty() {
            anyhow::bail!("product policy requires custom UEFI JSON");
        }
        self.validate_secure_boot_policy_enforcement()?;
        Ok(self.custom_uefi_json())
    }

    /// Enforce that the guest uses an ephemeral VMGS if required.
    fn enforce_ephemeral_vmgs_required(&self, vmgs_is_ephemeral: bool) -> anyhow::Result<()> {
        if self.require_ephemeral_vmgs() && !vmgs_is_ephemeral {
            anyhow::bail!("product policy requires an ephemeral VMGS guest state lifetime");
        }
        Ok(())
    }
}

/// Validate the secure boot policy from the parsed custom UEFI JSON.
fn validate_secure_boot_policy_enforcement_common<T: UefiSecurityPolicyParams + ?Sized>(
    params: &T,
) -> anyhow::Result<()> {
    use firmware_uefi_custom_vars::delta::SignaturesDelta;

    let delta = hyperv_uefi_custom_vars_json::load_delta_from_json(params.custom_uefi_json())
        .map_err(|e| anyhow::anyhow!("failed to parse custom UEFI JSON: {e}"))?;

    let sigs = match delta.signatures {
        SignaturesDelta::Replace(r) => r,
        SignaturesDelta::Append(_) => {
            anyhow::bail!("product policy requires Replace mode for secure boot signatures");
        }
    };

    if params.require_secure_boot_vars() {
        use firmware_uefi_custom_vars::delta::SignatureDelta;
        use firmware_uefi_custom_vars::delta::SignatureDeltaVec;

        // All vars must carry explicit signatures; Default relies on a base template.
        if matches!(sigs.pk, SignatureDelta::Default) {
            anyhow::bail!("product policy: PK uses Default (not self-contained)");
        }
        if matches!(sigs.kek, SignatureDeltaVec::Default) {
            anyhow::bail!("product policy: KEK uses Default (not self-contained)");
        }
        if matches!(sigs.db, SignatureDeltaVec::Default) {
            anyhow::bail!("product policy: db uses Default (not self-contained)");
        }
        if matches!(sigs.dbx, SignatureDeltaVec::Default) {
            anyhow::bail!("product policy: dbx uses Default (not self-contained)");
        }
    }

    if params.require_bcd_integrity() {
        use uefi_specs::uefi::nvram::vars::EFI_GLOBAL_VARIABLE;

        let has_bcd_hash = delta.custom_vars.iter().any(|(name, value)| {
            name == "BootConfigurationDataHash" && value.guid == EFI_GLOBAL_VARIABLE
        });
        if !has_bcd_hash {
            anyhow::bail!(
                "product policy: require_bcd_integrity is set but BootConfigurationDataHash variable is missing from custom UEFI JSON"
            );
        }
    }

    Ok(())
}
