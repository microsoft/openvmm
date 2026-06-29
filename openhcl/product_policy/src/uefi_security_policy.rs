// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! UEFI enforced security settings: traits and validation logic.
//!
//! This module is the single home for all UEFI-security-related
//! abstractions used by product policy variants (Sivm, Cwcow, etc.).

/// Internal trait providing access to policy fields needed by the
/// shared validation logic. Kept crate-private so raw getters are not
/// exposed outside the crate.
pub(crate) trait UefiSecurityPolicyParams {
    fn require_secure_boot(&self) -> bool;
    fn require_secure_boot_vars(&self) -> bool;
    fn require_bcd_integrity(&self) -> bool;
    fn custom_uefi_json(&self) -> &[u8];
    fn enforce_ephemeral_vmgs(&self) -> bool;
}

/// A trait for validating UEFI security settings. Implementors only
/// need to provide [`UefiSecurityPolicyParams`]; all methods here have
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

    /// Validate the secure boot policy enforcement.
    fn validate_secure_boot_policy_enforcement(&self) -> anyhow::Result<()> {
        validate_secure_boot_policy_enforcement(self)
    }

    /// Get the validated UEFI JSON.
    fn get_validated_uefi_json(&self) -> anyhow::Result<&[u8]> {
        if self.custom_uefi_json().is_empty() {
            anyhow::bail!("product policy requires custom UEFI JSON");
        }
        self.validate_secure_boot_policy_enforcement()?;
        Ok(self.custom_uefi_json())
    }

    /// Enforce that the guest uses an ephemeral VMGS if required.
    fn enforce_ephemeral_vmgs_required(&self, vmgs_is_ephemeral: bool) -> anyhow::Result<()> {
        if self.enforce_ephemeral_vmgs() && !vmgs_is_ephemeral {
            anyhow::bail!("product policy requires an ephemeral VMGS guest state lifetime");
        }
        Ok(())
    }
}

/// Validate the secure boot policy based on parsed custom UEFI JSON.
///
/// Enforces Replace mode (rejects Append), validates PK/KEK/db/dbx
/// carry explicit signatures when  is set,
/// and checks for  when
/// is set.
fn validate_secure_boot_policy_enforcement<T: UefiSecurityPolicyParams + ?Sized>(
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

        // All vars must carry explicit signatures —  relies on
        // a base template, which is not self-contained.
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

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;
    use crate::sivm::SivmPolicy;
    use alloc::string::ToString;
    use alloc::vec;

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

    #[test]
    fn get_validated_uefi_json_fails_on_empty() {
        let p = SivmPolicy {
            custom_uefi_json: vec![],
            ..Default::default()
        };
        let err = p.get_validated_uefi_json().unwrap_err();
        assert!(err.to_string().contains("custom UEFI JSON"));
    }

    #[test]
    fn enforcement_rejects_unparseable_json() {
        let p = SivmPolicy {
            require_secure_boot_vars: true,
            custom_uefi_json: vec![0xFF, 0xFE],
            ..Default::default()
        };
        let err = p.validate_secure_boot_policy_enforcement().unwrap_err();
        assert!(err.to_string().contains("failed to parse"));
    }

    /// Valid Replace-mode JSON with explicit PK/KEK/db/dbx.
    const REPLACE_JSON: &[u8] = br#"{
    "type": "Microsoft.Compute/disks",
    "properties": {
        "uefiSettings": {
            "signatureMode": "Replace",
            "signatures": {
                "PK": {
                    "type": "x509",
                    "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="]
                },
                "KEK": [{
                    "type": "x509",
                    "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="]
                }],
                "db": [{
                    "type": "x509",
                    "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="]
                }],
                "dbx": [{
                    "type": "sha256",
                    "value": ["AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="]
                }]
            }
        }
    }
}"#;

    /// Valid Replace-mode JSON with BCD hash custom variable.
    const REPLACE_JSON_WITH_BCD: &[u8] = br#"{
    "type": "Microsoft.Compute/disks",
    "properties": {
        "uefiSettings": {
            "signatureMode": "Replace",
            "signatures": {
                "PK": {
                    "type": "x509",
                    "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="]
                },
                "KEK": [{
                    "type": "x509",
                    "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="]
                }],
                "db": [{
                    "type": "x509",
                    "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="]
                }],
                "dbx": [{
                    "type": "sha256",
                    "value": ["AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="]
                }]
            },
            "BootConfigurationDataHash": {
                "guid": "Yd/ki8qT0hGqDQDgmAMrjA==",
                "attributes": "BwAAAA==",
                "value": "aGFzaHZhbHVl"
            }
        }
    }
}"#;

    /// Replace-mode JSON with a `BootConfigurationDataHash` variable that uses a
    /// non-global (wrong) namespace GUID instead of `EFI_GLOBAL_VARIABLE`.
    const REPLACE_JSON_WITH_BCD_WRONG_GUID: &[u8] = br#"{
    "type": "Microsoft.Compute/disks",
    "properties": {
        "uefiSettings": {
            "signatureMode": "Replace",
            "signatures": {
                "PK": {
                    "type": "x509",
                    "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="]
                },
                "KEK": [{
                    "type": "x509",
                    "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="]
                }],
                "db": [{
                    "type": "x509",
                    "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="]
                }],
                "dbx": [{
                    "type": "sha256",
                    "value": ["AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="]
                }]
            },
            "BootConfigurationDataHash": {
                "guid": "vZr6d1kDTTK9YCj05494Sw==",
                "attributes": "BwAAAA==",
                "value": "aGFzaHZhbHVl"
            }
        }
    }
}"#;

    /// Append-mode JSON.
    const APPEND_JSON: &[u8] = br#"{
    "type": "Microsoft.Compute/disks",
    "properties": {
        "uefiSettings": {
            "signatureMode": "Append",
            "signatures": {
                "KEK": [{
                    "type": "x509",
                    "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="]
                }]
            }
        }
    }
}"#;

    #[test]
    fn enforcement_rejects_append_mode() {
        let p = SivmPolicy {
            require_secure_boot_vars: true,
            custom_uefi_json: APPEND_JSON.to_vec(),
            ..Default::default()
        };
        let err = p.validate_secure_boot_policy_enforcement().unwrap_err();
        assert!(err.to_string().contains("Replace mode"));
    }

    #[test]
    fn enforcement_passes_valid_replace_json() {
        let p = SivmPolicy {
            require_secure_boot_vars: true,
            require_bcd_integrity: false,
            custom_uefi_json: REPLACE_JSON.to_vec(),
            ..Default::default()
        };
        assert!(p.validate_secure_boot_policy_enforcement().is_ok());
    }

    #[test]
    fn bcd_integrity_fails_when_hash_missing() {
        let p = SivmPolicy {
            require_bcd_integrity: true,
            custom_uefi_json: REPLACE_JSON.to_vec(),
            ..Default::default()
        };
        let err = p.validate_secure_boot_policy_enforcement().unwrap_err();
        assert!(err.to_string().contains("BootConfigurationDataHash"));
    }

    #[test]
    fn bcd_integrity_passes_when_hash_present() {
        let p = SivmPolicy {
            require_bcd_integrity: true,
            custom_uefi_json: REPLACE_JSON_WITH_BCD.to_vec(),
            ..Default::default()
        };
        assert!(p.validate_secure_boot_policy_enforcement().is_ok());
    }

    #[test]
    fn bcd_integrity_fails_when_hash_has_wrong_guid() {
        let p = SivmPolicy {
            require_bcd_integrity: true,
            custom_uefi_json: REPLACE_JSON_WITH_BCD_WRONG_GUID.to_vec(),
            ..Default::default()
        };
        let err = p.validate_secure_boot_policy_enforcement().unwrap_err();
        assert!(err.to_string().contains("BootConfigurationDataHash"));
    }

    /// Replace-mode JSON where `PK` relies on the template (`Default`).
    const REPLACE_JSON_PK_DEFAULT: &[u8] = br#"{
    "type": "Microsoft.Compute/disks",
    "properties": {
        "uefiSettings": {
            "signatureMode": "Replace",
            "signatures": {
                "PK": { "type": "Default" },
                "KEK": [{ "type": "x509", "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="] }],
                "db": [{ "type": "x509", "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="] }],
                "dbx": [{ "type": "sha256", "value": ["AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="] }]
            }
        }
    }
}"#;

    /// Replace-mode JSON where `KEK` relies on the template (`Default`).
    const REPLACE_JSON_KEK_DEFAULT: &[u8] = br#"{
    "type": "Microsoft.Compute/disks",
    "properties": {
        "uefiSettings": {
            "signatureMode": "Replace",
            "signatures": {
                "PK": { "type": "x509", "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="] },
                "KEK": [{ "type": "Default" }],
                "db": [{ "type": "x509", "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="] }],
                "dbx": [{ "type": "sha256", "value": ["AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="] }]
            }
        }
    }
}"#;

    /// Replace-mode JSON where `db` relies on the template (`Default`).
    const REPLACE_JSON_DB_DEFAULT: &[u8] = br#"{
    "type": "Microsoft.Compute/disks",
    "properties": {
        "uefiSettings": {
            "signatureMode": "Replace",
            "signatures": {
                "PK": { "type": "x509", "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="] },
                "KEK": [{ "type": "x509", "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="] }],
                "db": [{ "type": "Default" }],
                "dbx": [{ "type": "sha256", "value": ["AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="] }]
            }
        }
    }
}"#;

    /// Replace-mode JSON where `dbx` relies on the template (`Default`).
    const REPLACE_JSON_DBX_DEFAULT: &[u8] = br#"{
    "type": "Microsoft.Compute/disks",
    "properties": {
        "uefiSettings": {
            "signatureMode": "Replace",
            "signatures": {
                "PK": { "type": "x509", "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="] },
                "KEK": [{ "type": "x509", "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="] }],
                "db": [{ "type": "x509", "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="] }],
                "dbx": [{ "type": "Default" }]
            }
        }
    }
}"#;

    #[test]
    fn enforcement_rejects_pk_default_when_secure_boot_vars_required() {
        let p = SivmPolicy {
            require_secure_boot_vars: true,
            custom_uefi_json: REPLACE_JSON_PK_DEFAULT.to_vec(),
            ..Default::default()
        };
        let err = p.validate_secure_boot_policy_enforcement().unwrap_err();
        assert!(err.to_string().contains("PK uses Default"));
    }

    #[test]
    fn enforcement_rejects_kek_default_when_secure_boot_vars_required() {
        let p = SivmPolicy {
            require_secure_boot_vars: true,
            custom_uefi_json: REPLACE_JSON_KEK_DEFAULT.to_vec(),
            ..Default::default()
        };
        let err = p.validate_secure_boot_policy_enforcement().unwrap_err();
        assert!(err.to_string().contains("KEK uses Default"));
    }

    #[test]
    fn enforcement_rejects_db_default_when_secure_boot_vars_required() {
        let p = SivmPolicy {
            require_secure_boot_vars: true,
            custom_uefi_json: REPLACE_JSON_DB_DEFAULT.to_vec(),
            ..Default::default()
        };
        let err = p.validate_secure_boot_policy_enforcement().unwrap_err();
        assert!(err.to_string().contains("db uses Default"));
    }

    #[test]
    fn enforcement_rejects_dbx_default_when_secure_boot_vars_required() {
        let p = SivmPolicy {
            require_secure_boot_vars: true,
            custom_uefi_json: REPLACE_JSON_DBX_DEFAULT.to_vec(),
            ..Default::default()
        };
        let err = p.validate_secure_boot_policy_enforcement().unwrap_err();
        assert!(err.to_string().contains("dbx uses Default"));
    }

    /// JSON with no `uefiSettings` section.
    const JSON_MISSING_UEFI_SETTINGS: &[u8] = br#"{
    "type": "Microsoft.Compute/disks",
    "properties": {}
}"#;

    /// Replace-mode JSON with an empty `signatures` object (no PK/KEK/db/dbx).
    const JSON_EMPTY_SIGNATURES: &[u8] = br#"{
    "type": "Microsoft.Compute/disks",
    "properties": {
        "uefiSettings": {
            "signatureMode": "Replace",
            "signatures": {}
        }
    }
}"#;

    #[test]
    fn enforcement_rejects_missing_uefi_settings() {
        let p = SivmPolicy {
            require_secure_boot_vars: true,
            custom_uefi_json: JSON_MISSING_UEFI_SETTINGS.to_vec(),
            ..Default::default()
        };
        let err = p.validate_secure_boot_policy_enforcement().unwrap_err();
        assert!(err.to_string().contains("failed to parse"));
    }

    #[test]
    fn enforcement_rejects_empty_signatures() {
        let p = SivmPolicy {
            require_secure_boot_vars: true,
            custom_uefi_json: JSON_EMPTY_SIGNATURES.to_vec(),
            ..Default::default()
        };
        let err = p.validate_secure_boot_policy_enforcement().unwrap_err();
        assert!(err.to_string().contains("failed to parse"));
    }
}
