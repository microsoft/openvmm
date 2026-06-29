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
    /// Require an ephemeral VMGS (the attached VMGS must never be read).
    #[mesh(1)]
    pub enforce_ephemeral_vmgs: bool,

    /// Refuse to boot unless secure boot is enabled.
    #[mesh(2)]
    pub require_secure_boot: bool,

    /// Refuse to boot unless PK/KEK/db/dbx variables are present.
    #[mesh(3)]
    pub require_secure_boot_vars: bool,

    /// Refuse to boot unless `BootConfigurationDataHash` is set.
    #[mesh(4)]
    pub require_bcd_integrity: bool,

    /// Custom UEFI JSON bytes. Base64 in manifest JSON; mandatory.
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

    fn require_secure_boot_vars(&self) -> bool {
        self.require_secure_boot_vars
    }

    fn require_bcd_integrity(&self) -> bool {
        self.require_bcd_integrity
    }

    fn custom_uefi_json(&self) -> &[u8] {
        &self.custom_uefi_json
    }

    fn enforce_ephemeral_vmgs(&self) -> bool {
        self.enforce_ephemeral_vmgs
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
