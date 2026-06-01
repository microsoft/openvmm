// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CWCOW (Confidential Windows Container on Windows) view.
//!
//! Usage:
//! ```ignore
//! openhcl_product_policy::cwcow::policy().validate_secure_boot_enabled(on)?;
//! ```


extern crate alloc;

use alloc::vec::Vec;

/// CWCOW policy body.
#[derive(mesh_protobuf::Protobuf, Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "manifest", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "manifest",
    serde(rename_all = "snake_case", deny_unknown_fields)
)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
#[mesh(package = "openhcl.product_policy")]
pub struct CwcowPolicy {
    /// Enforce read-only mode for the VMGS partition. With this
    /// set, OpenHCL refuses writes to the VMGS (including any
    /// host-initiated change attempt).
    #[mesh(1)]
    pub vmgs_read_only: bool,

    /// Require secure-boot-only mode: refuse to boot if secure
    /// boot is not enabled.
    #[mesh(2)]
    pub require_secure_boot: bool,

    /// Require the presence of secure boot variables (PK, KEK,
    /// db, dbx, etc.) in the UEFI nvram. Builds without the
    /// expected variables are refused.
    #[mesh(3)]
    pub require_secure_boot_vars: bool,

    /// Require the `BootConfigurationDataHash` UEFI variable to
    /// be set via the custom UEFI JSON below, providing BCD
    /// integrity at boot.
    #[mesh(4)]
    pub require_bcd_integrity: bool,

    /// Require Secure AVIC to be enabled on platforms that
    /// support it (currently Turin SNP). OpenHCL refuses to
    /// continue if this is set but Secure AVIC is disabled.
    #[mesh(5)]
    pub require_secure_avic: bool,

    /// Custom UEFI JSON bytes. Encoded as standard base64 in
    /// manifest JSON. Mandatory and must be non-empty — an empty
    /// value panics in `encode_product_policy_bytes`.
    #[mesh(6)]
    #[cfg_attr(feature = "manifest", serde(with = "custom_uefi_json_serde"))]
    #[cfg_attr(feature = "inspect", inspect(with = "Vec::<u8>::len"))]
    pub custom_uefi_json: Vec<u8>,
}

#[cfg(feature = "manifest")]
mod custom_uefi_json_serde {
    extern crate alloc;

    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;
    use base64::Engine as _;
    use serde::Deserialize as _;

    pub fn serialize<S>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        s.serialize_str(&encoded)
    }

    pub fn deserialize<'de, D>(d: D) -> Result<Vec<u8>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        if s.is_empty() {
            return Ok(Vec::new());
        }
        base64::engine::general_purpose::STANDARD
            .decode(s.as_bytes())
            .map_err(|e| serde::de::Error::custom(format!("failed to base64-decode bytes: {e}")))
    }
}

// Generates `pub struct CwcowPolicyView<'a>`, scaffolding methods
// (`from_policy` / `empty` / `is_active` / `body`), `current()` and
// the module-level `pub fn policy()`.
crate::product_view!(CwcowPolicyView, CwcowPolicy, crate::wire::ProductPolicy::Cwcow);

#[cfg(feature = "std")]
impl<'a> CwcowPolicyView<'a> {
    /// Fail unless secure boot is enabled, when the CWCOW policy
    /// requires it. No-op when no CWCOW policy is in effect or when
    /// the policy does not require secure boot.
    ///
    /// Additional `validate_*` helpers (vmgs read-only, secure-boot
    /// variables, BCD integrity, Secure AVIC, custom UEFI JSON) land
    /// in follow-up commits alongside their consumer wire-up.
    pub fn validate_secure_boot_enabled(&self, on: bool) -> anyhow::Result<()> {
        if let Some(p) = self.body() {
            if p.require_secure_boot && !on {
                anyhow::bail!("CWCOW policy requires secure boot to be enabled");
            }
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    extern crate alloc;
    use super::*;
    use alloc::string::ToString;

    fn policy_no_requirements() -> CwcowPolicy {
        CwcowPolicy {
            custom_uefi_json: b"x".to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn empty_view_is_a_noop() {
        let v = CwcowPolicyView::empty();
        assert!(!v.is_active());
        assert!(v.validate_secure_boot_enabled(false).is_ok());
        assert!(v.validate_secure_boot_enabled(true).is_ok());
    }

    #[test]
    fn flag_off_means_arg_does_not_matter() {
        let body = policy_no_requirements();
        let v = CwcowPolicyView::from_policy(&body);
        assert!(v.is_active());
        assert!(v.validate_secure_boot_enabled(false).is_ok());
        assert!(v.validate_secure_boot_enabled(true).is_ok());
    }

    #[test]
    fn flag_on_passes_when_secure_boot_is_enabled() {
        let body = CwcowPolicy {
            require_secure_boot: true,
            custom_uefi_json: b"x".to_vec(),
            ..Default::default()
        };
        let v = CwcowPolicyView::from_policy(&body);
        assert!(v.validate_secure_boot_enabled(true).is_ok());
    }

    #[test]
    fn flag_on_fails_when_secure_boot_is_disabled() {
        let body = CwcowPolicy {
            require_secure_boot: true,
            custom_uefi_json: b"x".to_vec(),
            ..Default::default()
        };
        let v = CwcowPolicyView::from_policy(&body);
        let err = v.validate_secure_boot_enabled(false).unwrap_err();
        assert!(err.to_string().contains("secure boot"));
    }
}
