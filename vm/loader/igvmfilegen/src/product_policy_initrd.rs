// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Append measured product policy and allowed PCI devices to the OpenHCL initrd.

use anyhow::Context as _;
use igvmfilegen_config::DevicePolicyConfig;
use product_policy::ProductPolicy;

/// Appends `policy` to the OpenHCL initrd, with the devices from
/// `device_policy`.
pub fn append(
    initrd: &mut Vec<u8>,
    policy: &ProductPolicy,
    device_policy: Option<&DevicePolicyConfig>,
) -> anyhow::Result<()> {
    let mut policy = policy.clone();
    if let Some(device_policy) = device_policy {
        load_device_policy(device_policy, &mut policy)?;
    }
    let (require_secure_boot, require_vars, require_bcd, custom_uefi_json) = match &policy {
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
        ProductPolicy::Azure(_) => (false, false, false, &Vec::new()),
    };
    anyhow::ensure!(
        !(require_secure_boot && (require_vars || require_bcd) && custom_uefi_json.is_empty()),
        "product policy requires non-empty custom_uefi_json"
    );

    let bytes = product_policy::encode_product_policy(&policy);
    initrd_cpio::append_to_initrd(
        initrd,
        product_policy::PRODUCT_POLICY_PATH.trim_start_matches('/'),
        &bytes,
        0o100400,
    )
    .context("failed to append product policy to OpenHCL initrd")
}

/// Resolves an inline device policy's catalog entries into `policy`.
/// `"*"` allows every device.
fn load_device_policy(
    device_policy: &DevicePolicyConfig,
    policy: &mut ProductPolicy,
) -> anyhow::Result<()> {
    let devices_path = &device_policy.devices;
    let devices: std::collections::BTreeMap<String, product_policy::AllowedPciDevice> =
        toml_edit::de::from_str(
            &fs_err::read_to_string(devices_path)
                .with_context(|| format!("reading {}", devices_path.display()))?,
        )
        .with_context(|| format!("parsing {}", devices_path.display()))?;
    // An entry with no IDs matches every device; only "*" may produce one.
    let any = product_policy::AllowedPciDevice::default();
    let device = |name: &String| match devices.get(name) {
        _ if name == "*" => Ok(any.clone()),
        Some(device) if *device != any => Ok(device.clone()),
        _ => anyhow::bail!("{name:?} isn't defined or sets no IDs in {devices_path:?}"),
    };
    policy.set_vpci_relay(product_policy::VpciRelayPolicy {
        allowed_pci_devices: device_policy
            .allowed_pci_devices
            .iter()
            .map(device)
            .collect::<anyhow::Result<_>>()?,
        mode: device_policy.vpci_relay.unwrap_or_default(),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use product_policy::VpciRelayMode;
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
        append(&mut initrd, &old_policy, None).unwrap();
        let original = initrd.clone();

        let policy = ProductPolicy::Sivm(SivmPolicy {
            require_secure_boot: true,
            require_secure_boot_vars: true,
            custom_uefi_json: vec![0xA5; 8192],
            ..Default::default()
        });
        append(&mut initrd, &policy, None).unwrap();
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
        append(&mut initrd, &policy, None).unwrap();
        let files = extract_files(&initrd);
        let bytes = &files[product_policy::PRODUCT_POLICY_PATH.trim_start_matches('/')];
        assert_eq!(
            product_policy::decode_product_policy(bytes).unwrap(),
            policy
        );
    }

    /// The device policy decides which PCI devices OpenHCL may pass to the
    /// guest, and the encoded policy is part of the measured initrd. For each
    /// in-tree policy, resolving the manifest's device names through the
    /// catalog and encoding the result must give exactly the expected devices
    /// and relay mode, and bytes that decode back to the same policy. The
    /// Azure policies must allow the same devices OpenHCL allowed before
    /// device policies existed, the SIVM policy only the GPU; only CVMs run
    /// the relay by default, and only SIVMs require it.
    #[test]
    fn device_policy_round_trips_through_initrd() {
        let manifests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../manifests");
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
        let azure = vec![class(1, 8, 2), microsoft(0xba, 2), microsoft(0xc003, 0x10)];
        for (file, devices, mode) in [
            (
                "openhcl-x64-cvm-release.toml",
                azure.clone(),
                VpciRelayMode::DefaultOn,
            ),
            ("openhcl-x64-release.toml", azure, VpciRelayMode::DefaultOff),
            (
                "openhcl-x64-cvm-sivm-release.toml",
                vec![gpu],
                VpciRelayMode::MustBeOn,
            ),
        ] {
            let config: igvmfilegen_config::Config =
                toml_edit::de::from_str(&fs_err::read_to_string(manifests.join(file)).unwrap())
                    .unwrap();
            for guest in config.guest_configs {
                let igvmfilegen_config::Image::Openhcl {
                    product_policy: Some(config),
                    ..
                } = guest.image
                else {
                    panic!("expected OpenHCL product policy");
                };
                let mut device_policy = config.policy.device_policy().unwrap().clone();
                device_policy.devices = manifests.join(&device_policy.devices);
                let policy = config.policy.policy();
                let mut initrd = Vec::new();
                append(&mut initrd, &policy, Some(&device_policy)).unwrap();
                let files = extract_files(&initrd);
                let bytes = &files[product_policy::PRODUCT_POLICY_PATH.trim_start_matches('/')];
                let mut expected = policy;
                expected.set_vpci_relay(product_policy::VpciRelayPolicy {
                    allowed_pci_devices: devices.clone(),
                    mode,
                });
                assert_eq!(*bytes, product_policy::encode_product_policy(&expected));
                assert_eq!(
                    product_policy::decode_product_policy(bytes).unwrap(),
                    expected
                );
            }
        }
    }

    /// `"*"` is the one way to allow every device; it must become a single
    /// match-anything entry, not a lookup in the catalog. A policy without
    /// `vpci_relay` must get the documented default, `default_off`.
    #[test]
    fn inline_device_policy_wildcard_and_default_mode() {
        let device_policy = DevicePolicyConfig {
            devices: std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../manifests/policies/devices.toml"),
            allowed_pci_devices: vec!["*".into()],
            vpci_relay: None,
        };
        let mut policy = ProductPolicy::Azure(Default::default());
        load_device_policy(&device_policy, &mut policy).unwrap();
        let policy = product_policy::MeasuredProductPolicy::new(Some(policy));
        assert_eq!(
            policy.allowed_pci_devices(),
            [product_policy::AllowedPciDevice::default()]
        );
        assert_eq!(policy.vpci_relay(), VpciRelayMode::DefaultOff);
    }

    /// A device name missing from the catalog, or a missing catalog file,
    /// must fail the build with a clear error and leave the initrd untouched.
    /// Skipping the bad entry instead would build an image whose device list
    /// differs from what the manifest says.
    #[test]
    fn invalid_inline_device_policy_does_not_modify_initrd() {
        let catalog = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../manifests/policies/devices.toml");
        for (devices, name, message) in [
            (catalog.clone(), "undefined-device", "isn't defined"),
            (
                catalog.with_file_name("missing-devices.toml"),
                "nvme",
                "reading",
            ),
        ] {
            let device_policy = DevicePolicyConfig {
                devices,
                allowed_pci_devices: vec![name.into()],
                vpci_relay: Some(VpciRelayMode::MustBeOn),
            };
            let mut initrd = vec![1, 2, 3];
            let error = append(
                &mut initrd,
                &ProductPolicy::Azure(Default::default()),
                Some(&device_policy),
            )
            .unwrap_err();
            assert!(error.to_string().contains(message), "{error:#}");
            assert_eq!(initrd, [1, 2, 3]);
        }
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
            let error = append(&mut initrd, &policy, None).unwrap_err();
            assert!(error.to_string().contains("non-empty custom_uefi_json"));
            assert_eq!(initrd, [1, 2, 3]);
        }
    }
}
