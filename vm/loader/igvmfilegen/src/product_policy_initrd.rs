// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Append measured product policy and allowed PCI devices to the OpenHCL initrd.

use anyhow::Context as _;
use product_policy::ProductPolicy;

pub fn append(
    initrd: &mut Vec<u8>,
    policy: Option<&ProductPolicy>,
    device_policy: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let devices = device_policy.map_or(Ok(Vec::new()), load_devices)?;
    let (require_secure_boot, require_vars, require_bcd, custom_uefi_json) = match policy {
        Some(ProductPolicy::Sivm(p)) => (
            p.require_secure_boot,
            p.require_secure_boot_vars,
            p.require_bcd_integrity,
            &p.custom_uefi_json,
        ),
        Some(ProductPolicy::Cwcow(p)) => (
            p.require_secure_boot,
            p.require_secure_boot_vars,
            p.require_bcd_integrity,
            &p.custom_uefi_json,
        ),
        None if devices.is_empty() => return Ok(()),
        None => (false, false, false, &Vec::new()),
    };
    anyhow::ensure!(
        !(require_secure_boot && (require_vars || require_bcd) && custom_uefi_json.is_empty()),
        "product policy requires non-empty custom_uefi_json"
    );

    let bytes = product_policy::encode_product_policy(policy, &devices);
    initrd_cpio::append_to_initrd(
        initrd,
        product_policy::PRODUCT_POLICY_PATH.trim_start_matches('/'),
        &bytes,
        0o100400,
    )
    .context("failed to append product policy to OpenHCL initrd")
}

/// Loads the devices that a device policy (see `vm/loader/manifests/policies`)
/// allows; `"*"` allows every device.
fn load_devices(path: &std::path::Path) -> anyhow::Result<Vec<product_policy::AllowedPciDevice>> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Policy {
        devices: std::path::PathBuf,
        allowed_pci_devices: Vec<String>,
    }
    let policy: Policy = toml_edit::de::from_str(&fs_err::read_to_string(path)?)
        .with_context(|| format!("parsing {}", path.display()))?;
    let devices_path = path.with_file_name(&policy.devices);
    let devices: std::collections::BTreeMap<String, product_policy::AllowedPciDevice> =
        toml_edit::de::from_str(&fs_err::read_to_string(&devices_path)?)
            .with_context(|| format!("parsing {}", devices_path.display()))?;
    // An entry with no IDs matches every device; only "*" may produce one.
    let any = product_policy::AllowedPciDevice::default();
    let device = |name: &String| match devices.get(name) {
        _ if name == "*" => Ok(any.clone()),
        Some(device) if *device != any => Ok(device.clone()),
        _ => anyhow::bail!("{name:?} isn't defined or sets no IDs in {devices_path:?}"),
    };
    policy.allowed_pci_devices.iter().map(device).collect()
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
        append(&mut initrd, Some(&old_policy), None).unwrap();
        let original = initrd.clone();

        let policy = ProductPolicy::Sivm(SivmPolicy {
            require_secure_boot: true,
            require_secure_boot_vars: true,
            custom_uefi_json: vec![0xA5; 8192],
            ..Default::default()
        });
        append(&mut initrd, Some(&policy), None).unwrap();
        assert_eq!(&initrd[..original.len()], &original);
        let files = extract_files(&initrd);
        assert_eq!(files["init"], b"existing init");
        assert_eq!(files["extra-layer"], b"extra data");
        let bytes = &files[product_policy::PRODUCT_POLICY_PATH.trim_start_matches('/')];
        assert_eq!(
            product_policy::decode_product_policy(bytes).unwrap(),
            product_policy::MeasuredProductPolicy::new(Some(policy))
        );
    }

    #[test]
    fn cwcow_policy_round_trips_through_initrd() {
        let policy = ProductPolicy::Cwcow(CwcowPolicy {
            require_secure_boot: true,
            ..Default::default()
        });
        let mut initrd = Vec::new();
        append(&mut initrd, Some(&policy), None).unwrap();
        let files = extract_files(&initrd);
        let bytes = &files[product_policy::PRODUCT_POLICY_PATH.trim_start_matches('/')];
        assert_eq!(
            product_policy::decode_product_policy(bytes).unwrap(),
            product_policy::MeasuredProductPolicy::new(Some(policy))
        );
    }

    /// The Azure device policy must allow exactly the devices that OpenHCL used
    /// to allow unconditionally.
    #[test]
    fn device_policy_round_trips_through_initrd() {
        let policies =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../manifests/policies");
        let mut initrd = Vec::new();
        append(&mut initrd, None, None).unwrap();
        assert!(initrd.is_empty());
        append(&mut Vec::new(), None, Some(&policies.join("sivm.toml"))).unwrap();
        append(&mut initrd, None, Some(&policies.join("azure.toml"))).unwrap();
        let files = extract_files(&initrd);
        let bytes = &files[product_policy::PRODUCT_POLICY_PATH.trim_start_matches('/')];
        let policy = product_policy::decode_product_policy(bytes).unwrap();
        let class = |base_class, sub_class, prog_if| product_policy::AllowedPciDevice {
            base_class: Some(base_class),
            sub_class: Some(sub_class),
            prog_if: Some(prog_if),
            ..Default::default()
        };
        let microsoft = |device_id, base_class| product_policy::AllowedPciDevice {
            vendor_id: Some(0x1414),
            device_id: Some(device_id),
            ..class(base_class, 0, 0)
        };
        let azure = [class(1, 8, 2), microsoft(0xba, 2), microsoft(0xc003, 0x10)];
        assert_eq!(
            (policy.raw(), policy.allowed_pci_devices()),
            (None, &azure[..])
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
            let error = append(&mut initrd, Some(&policy), None).unwrap_err();
            assert!(error.to_string().contains("non-empty custom_uefi_json"));
            assert_eq!(initrd, [1, 2, 3]);
        }
    }
}
