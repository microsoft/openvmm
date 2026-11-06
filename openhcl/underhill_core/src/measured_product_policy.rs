// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Load measured product policy from the initrd and validate the VM.

use crate::dispatch::LoadedVm;
use anyhow::Context as _;
use cvm_tracing::CVM_ALLOWED;
use product_policy::MeasuredProductPolicy;
use product_policy::UefiSecurityPolicy;

pub fn load() -> anyhow::Result<MeasuredProductPolicy> {
    load_from_path(std::path::Path::new(product_policy::PRODUCT_POLICY_PATH))
}

fn load_from_path(path: &std::path::Path) -> anyhow::Result<MeasuredProductPolicy> {
    let bytes = match fs_err::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(CVM_ALLOWED, "no product policy installed");
            return Ok(MeasuredProductPolicy::default());
        }
        Err(err) => return Err(err).context("failed to read product policy from initrd"),
    };

    decode(&bytes)
}

fn decode(buf: &[u8]) -> anyhow::Result<MeasuredProductPolicy> {
    let policy = product_policy::decode_product_policy(buf)
        .map_err(anyhow::Error::from)
        .context("product policy decode failed")?;

    // Integrity check to ensure we are enforcing the complete policy
    let encoded_len = product_policy::encode_product_policy(&policy).len();
    if encoded_len != buf.len() {
        anyhow::bail!(
            "product policy size mismatch: file has {} bytes, re-encoded {encoded_len} bytes",
            buf.len(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn missing_policy_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let policy = load_from_path(&dir.path().join("missing")).unwrap();
        assert_eq!(policy, MeasuredProductPolicy::default());
    }

    #[test]
    fn read_errors_are_not_treated_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let error = load_from_path(dir.path()).unwrap_err();
        assert!(error.to_string().contains("failed to read product policy"));
    }

    #[test]
    fn loads_policy_file_larger_than_measured_config_page() {
        let policy = product_policy::ProductPolicy::Sivm(product_policy::sivm::SivmPolicy {
            require_secure_boot: true,
            custom_uefi_json: vec![0xA5; 8192],
            ..Default::default()
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy");
        fs_err::write(&path, product_policy::encode_product_policy(&policy)).unwrap();
        assert_eq!(load_from_path(&path).unwrap().raw(), Some(&policy));
    }

    #[test]
    fn malformed_policy_files_fail() {
        let policy = product_policy::ProductPolicy::Sivm(product_policy::sivm::SivmPolicy {
            custom_uefi_json: vec![1, 2, 3],
            ..Default::default()
        });
        let encoded = product_policy::encode_product_policy(&policy);
        let mut unknown_field = encoded.clone();
        unknown_field.extend_from_slice(&[0x18, 1]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy");
        for bytes in [
            &[][..],
            &[0xFF, 0xFE][..],
            &encoded[..encoded.len() - 1],
            &unknown_field,
        ] {
            fs_err::write(&path, bytes).unwrap();
            assert!(load_from_path(&path).is_err());
        }
    }
}
