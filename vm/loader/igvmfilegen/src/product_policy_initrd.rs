// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Append measured product policy to the OpenHCL initrd.

use anyhow::Context as _;
use product_policy::ProductPolicy;

pub fn append(initrd: &mut Vec<u8>, policy: &ProductPolicy) -> anyhow::Result<()> {
    let (require_secure_boot, require_vars, require_bcd, custom_uefi_json) = match policy {
        ProductPolicy::Sivm(p) => (
            p.require_secure_boot,
            p.require_secure_boot_vars,
            p.require_bcd_integrity,
            &p.custom_uefi_json,
        ),
        ProductPolicy::Cwcow(p) => (
            p.require_secure_boot,
            p.require_secure_boot_vars,
            p.require_bcd_integrity,
            &p.custom_uefi_json,
        ),
    };
    anyhow::ensure!(
        !(require_secure_boot && (require_vars || require_bcd) && custom_uefi_json.is_empty()),
        "product policy requires non-empty custom_uefi_json"
    );

    let bytes = product_policy::encode_product_policy(policy);
    initrd_cpio::append_to_initrd(
        initrd,
        product_policy::PRODUCT_POLICY_PATH.trim_start_matches('/'),
        &bytes,
        0o100400,
    )
    .context("failed to append product policy to OpenHCL initrd")
}

#[cfg(test)]
mod tests {
    use super::*;
    use product_policy::cwcow::CwcowPolicy;
    use product_policy::sivm::SivmPolicy;
    use std::collections::BTreeMap;
    use std::io::Read as _;
    use test_with_tracing::test;

    fn extract_files(initrd: &[u8]) -> BTreeMap<String, Vec<u8>> {
        let mut archive = Vec::new();
        flate2::read::MultiGzDecoder::new(initrd)
            .read_to_end(&mut archive)
            .unwrap();
        let mut files = BTreeMap::new();
        let mut offset = 0;
        while offset < archive.len() {
            let header = &archive[offset..offset + 110];
            assert_eq!(&header[..6], b"070701");
            let field = |start| {
                usize::from_str_radix(std::str::from_utf8(&header[start..start + 8]).unwrap(), 16)
                    .unwrap()
            };
            let size = field(54);
            let name_size = field(94);
            let name_start = offset + 110;
            let name = std::str::from_utf8(&archive[name_start..name_start + name_size - 1])
                .unwrap()
                .to_owned();
            assert_eq!(archive[name_start + name_size - 1], 0);
            offset = (name_start + name_size).next_multiple_of(4);
            if name != "TRAILER!!!" {
                assert_eq!(field(14), 0o100400);
                files.insert(name, archive[offset..offset + size].to_vec());
            }
            offset = (offset + size).next_multiple_of(4);
        }
        files
    }

    #[test]
    fn appended_policy_preserves_layers_and_overrides_previous_policy() {
        let mut initrd = Vec::new();
        initrd_cpio::append_to_initrd(&mut initrd, "init", b"existing init", 0o100400).unwrap();
        initrd_cpio::append_to_initrd(&mut initrd, "extra-layer", b"extra data", 0o100400).unwrap();
        let old_policy = ProductPolicy::Sivm(SivmPolicy::default());
        append(&mut initrd, &old_policy).unwrap();
        let original = initrd.clone();

        let policy = ProductPolicy::Sivm(SivmPolicy {
            require_secure_boot: true,
            require_secure_boot_vars: true,
            custom_uefi_json: vec![0xA5; 8192],
            ..Default::default()
        });
        append(&mut initrd, &policy).unwrap();
        assert_eq!(&initrd[..original.len()], &original);
        let files = extract_files(&initrd);
        assert_eq!(files["init"], b"existing init");
        assert_eq!(files["extra-layer"], b"extra data");
        let bytes = &files[product_policy::PRODUCT_POLICY_PATH.trim_start_matches('/')];
        assert_eq!(
            product_policy::decode_product_policy(bytes).unwrap(),
            policy
        );
    }

    #[test]
    fn cwcow_policy_round_trips_through_initrd() {
        let policy = ProductPolicy::Cwcow(CwcowPolicy {
            require_secure_boot: true,
            ..Default::default()
        });
        let mut initrd = Vec::new();
        append(&mut initrd, &policy).unwrap();
        let files = extract_files(&initrd);
        let bytes = &files[product_policy::PRODUCT_POLICY_PATH.trim_start_matches('/')];
        assert_eq!(
            product_policy::decode_product_policy(bytes).unwrap(),
            policy
        );
    }

    #[test]
    fn invalid_policy_does_not_modify_initrd() {
        for policy in [
            ProductPolicy::Sivm(SivmPolicy {
                require_secure_boot: true,
                require_secure_boot_vars: true,
                ..Default::default()
            }),
            ProductPolicy::Cwcow(CwcowPolicy {
                require_secure_boot: true,
                require_bcd_integrity: true,
                ..Default::default()
            }),
        ] {
            let mut initrd = vec![1, 2, 3];
            let error = append(&mut initrd, &policy).unwrap_err();
            assert!(error.to_string().contains("non-empty custom_uefi_json"));
            assert_eq!(initrd, [1, 2, 3]);
        }
    }
}
