// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Measured product policy integration: decode from the measured VTL2
//! config region and post-load validation. Only compiled when the
//! `product_policy` feature is enabled.

use crate::dispatch::LoadedVm;
use anyhow::Context as _;
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
    Ok(())
}

/// Post-load validation of the measured product policy.
pub fn validate(loaded_vm: &LoadedVm) -> anyhow::Result<()> {
    loaded_vm
        .measured_product_policy
        .sivm(|p| validate_uefi_security_policy(p, loaded_vm))?;
    loaded_vm
        .measured_product_policy
        .cwcow(|p| validate_uefi_security_policy(p, loaded_vm))?;
    Ok(())
}
