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
    /// Require an ephemeral VMGS guest state lifetime.
    #[mesh(1)]
    pub require_ephemeral_vmgs: bool,

    /// Require secure boot is enabled.
    #[mesh(2)]
    pub require_secure_boot: bool,

    /// Require PK/KEK/db/dbx variables to be self-contained.
    #[mesh(3)]
    pub require_secure_boot_vars: bool,

    /// Require `BootConfigurationDataHash`.
    #[mesh(4)]
    pub require_bcd_integrity: bool,

    /// Custom UEFI JSON bytes (base64 in manifest JSON); mandatory when
    /// secure boot plus secure-boot-vars or BCD-integrity are set.
    #[mesh(5)]
    #[cfg_attr(
        feature = "manifest",
        serde(with = "super::product_policy_helpers::custom_uefi_json_serde")
    )]
    #[cfg_attr(feature = "inspect", inspect(with = "Vec::<u8>::len"))]
    pub custom_uefi_json: Vec<u8>,

    /// Require Secure AVIC to be enabled.
    #[mesh(6)]
    pub require_secure_avic: bool,
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

    fn require_ephemeral_vmgs(&self) -> bool {
        self.require_ephemeral_vmgs
    }
}

impl crate::uefi_security_policy::UefiSecurityPolicy for CwcowPolicy {}

impl CwcowPolicy {
    /// Enforce that Secure AVIC is enabled if required by the policy.
    pub fn enforce_secure_avic(&self, on: bool) -> anyhow::Result<()> {
        if self.require_secure_avic && !on {
            anyhow::bail!("product policy requires Secure AVIC to be enabled");
        }
        Ok(())
    }
}
