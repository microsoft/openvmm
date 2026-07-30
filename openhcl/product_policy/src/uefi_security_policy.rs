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
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;
    use crate::sivm::SivmPolicy;
    use alloc::string::ToString;

    #[test]
    fn secure_boot_flag_off_passes_either_way() {
        let p = SivmPolicy::default();
        assert!(p.validate_secure_boot_enabled(false).is_ok());
        assert!(p.validate_secure_boot_enabled(true).is_ok());
    }

    #[test]
    fn secure_boot_flag_on_passes_when_enabled() {
        let p = SivmPolicy {
            require_secure_boot: true,
            ..Default::default()
        };
        assert!(p.validate_secure_boot_enabled(true).is_ok());
    }

    #[test]
    fn secure_boot_flag_on_fails_when_disabled() {
        let p = SivmPolicy {
            require_secure_boot: true,
            ..Default::default()
        };
        let err = p.validate_secure_boot_enabled(false).unwrap_err();
        assert!(err.to_string().contains("secure boot"));
    }
}
