// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

extern crate alloc;

use alloc::vec::Vec;

#[derive(mesh_protobuf::Protobuf, Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "manifest", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "manifest",
    serde(rename_all = "snake_case", deny_unknown_fields)
)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
#[mesh(package = "openhcl.product_policy")]
/// Cwcow policy
pub struct CwcowPolicy {
    /// Reserved: require an ephemeral VMGS. Not enforced at runtime yet.
    #[mesh(1)]
    pub enforce_ephemeral_vmgs: bool,

    /// Refuse to boot unless secure boot is enabled.
    #[mesh(2)]
    pub require_secure_boot: bool,

    /// Reserved: require PK/KEK/db/dbx variables. Not enforced at runtime yet.
    #[mesh(3)]
    pub require_secure_boot_vars: bool,

    /// Reserved: require `BootConfigurationDataHash`. Not enforced at runtime yet.
    #[mesh(4)]
    pub require_bcd_integrity: bool,

    /// Custom UEFI JSON bytes (base64 in manifest JSON). Required in
    /// manifests and asserted non-empty at build time when secure boot
    /// plus secure-boot-vars or BCD-integrity are set; not validated at
    /// runtime.
    #[mesh(5)]
    #[cfg_attr(
        feature = "manifest",
        serde(with = "super::product_policy_helpers::custom_uefi_json_serde")
    )]
    #[cfg_attr(feature = "inspect", inspect(with = "Vec::<u8>::len"))]
    pub custom_uefi_json: Vec<u8>,

    /// Enforce that Secure AVIC is enabled.
    #[mesh(6)]
    pub enforce_secure_avic_enabled: bool,
}

impl crate::uefi_security_policy::UefiSecurityPolicyParams for CwcowPolicy {
    fn require_secure_boot(&self) -> bool {
        self.require_secure_boot
    }
}

impl crate::uefi_security_policy::UefiSecurityPolicy for CwcowPolicy {}

impl CwcowPolicy {
    /// Enforce that Secure AVIC is enabled.
    pub fn enforce_secure_avic(&self, on: bool) -> anyhow::Result<()> {
        if self.enforce_secure_avic_enabled && !on {
            anyhow::bail!("Secure AVIC is required but not enabled");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn secure_avic_flag_off_passes_either_way() {
        let p = CwcowPolicy::default();
        assert!(p.enforce_secure_avic(false).is_ok());
        assert!(p.enforce_secure_avic(true).is_ok());
    }

    #[test]
    fn secure_avic_flag_on_passes_when_enabled() {
        let p = CwcowPolicy {
            enforce_secure_avic_enabled: true,
            ..Default::default()
        };
        assert!(p.enforce_secure_avic(true).is_ok());
    }

    #[test]
    fn secure_avic_flag_on_fails_when_disabled() {
        let p = CwcowPolicy {
            enforce_secure_avic_enabled: true,
            ..Default::default()
        };
        let err = p.enforce_secure_avic(false).unwrap_err();
        assert!(err.to_string().contains("Secure AVIC"));
    }
}
