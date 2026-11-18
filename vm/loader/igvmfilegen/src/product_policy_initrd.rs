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
    let (devices, require_vpci_relay, max_vpci_devices) =
        device_policy.map_or(Ok((Vec::new(), false, None)), load_device_policy)?;
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
        None if device_policy.is_none() => return Ok(()),
        None => (false, false, false, &Vec::new()),
    };
    anyhow::ensure!(
        !(require_secure_boot && (require_vars || require_bcd) && custom_uefi_json.is_empty()),
        "product policy requires non-empty custom_uefi_json"
    );

    let bytes = product_policy::encode_product_policy(
        policy,
        &devices,
        require_vpci_relay,
        max_vpci_devices,
    );
    initrd_cpio::append_to_initrd(
        initrd,
        product_policy::PRODUCT_POLICY_PATH.trim_start_matches('/'),
        &bytes,
        0o100400,
    )
    .context("failed to append product policy to OpenHCL initrd")
}

/// Loads the devices that a device policy (see `vm/loader/manifests/policies`)
/// allows, where `"*"` allows every device, whether it requires the VPCI
/// relay, and how many devices the relay makes room for.
fn load_device_policy(
    path: &std::path::Path,
) -> anyhow::Result<(Vec<product_policy::AllowedPciDevice>, bool, Option<u16>)> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Policy {
        devices: std::path::PathBuf,
        allowed_pci_devices: Vec<String>,
        #[serde(default)]
        require_vpci_relay: bool,
        max_vpci_devices: Option<u16>,
    }
    let policy: Policy = toml_edit::de::from_str(&fs_err::read_to_string(path)?)
        .with_context(|| format!("parsing {}", path.display()))?;
    // OpenHCL sizes the relay's MMIO window and ring memory from this count.
    anyhow::ensure!(
        policy.max_vpci_devices != Some(0)
            && (policy.allowed_pci_devices.is_empty() || policy.max_vpci_devices.is_some()),
        "{}: a policy that allows devices must set a nonzero max_vpci_devices",
        path.display()
    );
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
    let devices = policy
        .allowed_pci_devices
        .iter()
        .map(device)
        .collect::<anyhow::Result<_>>()?;
    Ok((devices, policy.require_vpci_relay, policy.max_vpci_devices))
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

    /// The Azure CVM device policy must allow exactly the devices that OpenHCL
    /// used to allow unconditionally, and the SIVM policy only NVMe and the
    /// GPU. Only the SIVM policy requires the VPCI relay.
    #[test]
    fn device_policy_round_trips_through_initrd() {
        let policies =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../manifests/policies");
        let decode = |initrd: &[u8]| {
            let files = extract_files(initrd);
            let bytes = &files[product_policy::PRODUCT_POLICY_PATH.trim_start_matches('/')];
            product_policy::decode_product_policy(bytes).unwrap()
        };
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
        let gpu = product_policy::AllowedPciDevice {
            vendor_id: Some(0x10de),
            device_id: Some(0x2704),
            revision_id: Some(0xa1),
            ..Default::default()
        };
        let mut initrd = Vec::new();
        append(&mut initrd, None, None).unwrap();
        assert!(initrd.is_empty());
        let mut sivm = Vec::new();
        append(&mut sivm, None, Some(&policies.join("sivm.toml"))).unwrap();
        let sivm = decode(&sivm);
        assert_eq!(
            (
                sivm.allowed_pci_devices(),
                sivm.require_vpci_relay(),
                sivm.max_vpci_devices()
            ),
            (&[class(1, 8, 2), gpu][..], true, Some(64))
        );
        append(&mut initrd, None, Some(&policies.join("azure-cvm.toml"))).unwrap();
        let policy = decode(&initrd);
        let azure = [class(1, 8, 2), microsoft(0xba, 2), microsoft(0xc003, 0x10)];
        assert_eq!(
            (
                policy.raw(),
                policy.allowed_pci_devices(),
                policy.require_vpci_relay(),
                policy.max_vpci_devices()
            ),
            (None, &azure[..], false, Some(64))
        );
    }

    /// A policy that allows devices must say how many the relay makes room
    /// for, and that can't be zero.
    #[test]
    fn device_policy_needs_a_device_count() {
        let dir = tempfile::tempdir().unwrap();
        fs_err::write(dir.path().join("devices.toml"), "[nvme]\nbase_class = 1\n").unwrap();
        let path = dir.path().join("policy.toml");
        let append_with = |count: &str| {
            let policy = "devices = \"devices.toml\"\nallowed_pci_devices = [\"nvme\"]\n";
            fs_err::write(&path, format!("{policy}{count}")).unwrap();
            append(&mut Vec::new(), None, Some(&path))
        };
        assert!(append_with("max_vpci_devices = 1\n").is_ok());
        assert!(append_with("").is_err());
        assert!(append_with("max_vpci_devices = 0\n").is_err());
    }

    /// An empty device policy still writes the policy file, so it replaces an
    /// allowlist from an earlier layer and OpenHCL allows no devices.
    #[test]
    fn empty_device_policy_replaces_previous_allowlist() {
        let policies =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../manifests/policies");
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.toml");
        fs_err::write(dir.path().join("devices.toml"), "").unwrap();
        fs_err::write(
            &empty,
            "devices = \"devices.toml\"\nallowed_pci_devices = []\n",
        )
        .unwrap();
        let mut initrd = Vec::new();
        append(&mut initrd, None, Some(&policies.join("azure-cvm.toml"))).unwrap();
        append(&mut initrd, None, Some(&empty)).unwrap();
        let files = extract_files(&initrd);
        let bytes = &files[product_policy::PRODUCT_POLICY_PATH.trim_start_matches('/')];
        assert_eq!(
            product_policy::decode_product_policy(bytes).unwrap(),
            product_policy::MeasuredProductPolicy::default()
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
