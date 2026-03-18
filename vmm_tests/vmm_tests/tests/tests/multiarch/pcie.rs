// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::multiarch::OsFlavor;
use crate::multiarch::cmd;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use pipette_client::PipetteClient;
use std::fmt;
use vmm_test_macros::openvmm_test;

struct ParsedPciDevice {
    vendor_id: u16,
    device_id: u16,
    class_code: u32,
}

impl fmt::Debug for ParsedPciDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParsedPciDevice")
            .field("vendor_id", &format_args!("0x{:X}", self.vendor_id))
            .field("device_id", &format_args!("0x{:X}", self.device_id))
            .field("class_code", &format_args!("0x{:X}", self.class_code))
            .finish()
    }
}

async fn parse_guest_pci_devices(
    os_flavor: OsFlavor,
    agent: &PipetteClient,
) -> anyhow::Result<Vec<ParsedPciDevice>> {
    let mut devs = vec![];
    match os_flavor {
        OsFlavor::Linux => {
            const PCI_SYSFS_PATH: &str = "/sys/bus/pci/devices";
            let sh = agent.unix_shell();
            let ls_output = cmd!(sh, "ls {PCI_SYSFS_PATH}").read().await?;
            let ls_devices = ls_output.as_str().lines();

            for ls_device in ls_devices {
                let device_sysfs_path = format!("{PCI_SYSFS_PATH}/{ls_device}");

                let vendor_output = cmd!(sh, "cat {device_sysfs_path}/vendor").read().await?;
                let vendor_id = u16::from_str_radix(vendor_output.strip_prefix("0x").unwrap(), 16)?;

                let device_output = cmd!(sh, "cat {device_sysfs_path}/device").read().await?;
                let device_id = u16::from_str_radix(device_output.strip_prefix("0x").unwrap(), 16)?;

                let class_output = cmd!(sh, "cat {device_sysfs_path}/class").read().await?;
                let class_code = u32::from_str_radix(class_output.strip_prefix("0x").unwrap(), 16)?;

                devs.push(ParsedPciDevice {
                    vendor_id,
                    device_id,
                    class_code,
                });
            }
        }
        OsFlavor::Windows => {
            let sh = agent.windows_shell();
            let output = cmd!(
                sh,
                "pnputil.exe /enum-devices /bus PCI /connected /properties"
            )
            .read()
            .await?;

            let lines = output.as_str().lines();
            let mut parsing_hwids = false;
            for line in lines {
                if parsing_hwids {
                    // Find one matching PCI\VEN_XXXX&DEV_YYYY&CC_ZZZZZZ
                    let mut toks = line.trim().split('_');
                    if let (Some(tok0), Some(tok1), Some(tok2), Some(tok3)) =
                        (toks.next(), toks.next(), toks.next(), toks.next())
                    {
                        if tok0.ends_with("VEN")
                            && tok1.ends_with("DEV")
                            && tok2.ends_with("CC")
                            && tok3.len() == 6
                        {
                            let vendor_id = u16::from_str_radix(&tok1[..4], 16)?;
                            let device_id = u16::from_str_radix(&tok2[..4], 16)?;
                            let class_code = u32::from_str_radix(&tok3[..6], 16)?;
                            devs.push(ParsedPciDevice {
                                vendor_id,
                                device_id,
                                class_code,
                            });
                            parsing_hwids = false;
                        }
                    }
                } else if line.contains("DEVPKEY_Device_HardwareIds") {
                    parsing_hwids = true;
                } else if line.contains("DEVPKEY") {
                    parsing_hwids = false;
                }
            }
        }
        _ => unreachable!(),
    }

    Ok(devs)
}

#[openvmm_test(
    linux_direct_x64,
    uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    uefi_x64(vhd(ubuntu_2404_server_x64)),
    uefi_aarch64(vhd(windows_11_enterprise_aarch64))
    // uefi_aarch64(vhd(ubuntu_2404_server_aarch64))
)]
async fn pcie_root_emulation_single_segment(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (vm, agent) = config
        .modify_backend(|b| b.with_pcie_topology(1, 4, 4))
        .run()
        .await?;

    let guest_devices = parse_guest_pci_devices(os_flavor, &agent).await?;
    tracing::info!(?guest_devices, "guest devices");

    let root_port_count = guest_devices
        .iter()
        .filter(|d| d.vendor_id == 0x1414 && d.device_id == 0xc030 && d.class_code == 0x060400)
        .count();

    assert_eq!(root_port_count, 1 * 4 * 4);

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

#[openvmm_test(
    linux_direct_x64,
    uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    uefi_x64(vhd(ubuntu_2404_server_x64)),
    uefi_aarch64(vhd(windows_11_enterprise_aarch64))
    // uefi_aarch64(vhd(ubuntu_2404_server_aarch64))
)]
async fn pcie_root_emulation_multi_segment(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (vm, agent) = config
        .modify_backend(|b| b.with_pcie_topology(4, 1, 8))
        .run()
        .await?;

    let guest_devices = parse_guest_pci_devices(os_flavor, &agent).await?;
    tracing::info!(?guest_devices, "guest devices");

    let root_port_count = guest_devices
        .iter()
        .filter(|d| d.vendor_id == 0x1414 && d.device_id == 0xc030 && d.class_code == 0x060400)
        .count();

    assert_eq!(root_port_count, 4 * 1 * 8);

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

#[openvmm_test(
    uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    uefi_x64(vhd(ubuntu_2404_server_x64)),
    uefi_aarch64(vhd(windows_11_enterprise_aarch64))
    // uefi_aarch64(vhd(ubuntu_2404_server_aarch64))
)]
async fn pcie_devices(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (vm, agent) = config
        .modify_backend(|b| b.with_pcie_topology(1, 1, 8).with_pcie_nvme("s0rc0rp0").with_pcie_nic("s0rc0rp1"))
        .run()
        .await?;

    let guest_devices = parse_guest_pci_devices(os_flavor, &agent).await?;
    tracing::info!(?guest_devices, "guest devices");

    let root_port_count = guest_devices
        .iter()
        .filter(|d| d.vendor_id == 0x1414 && d.device_id == 0xc030 && d.class_code == 0x060400)
        .count();

    assert_eq!(root_port_count, 8);

    let nvme_count = guest_devices
        .iter()
        .filter(|d| d.class_code == 0x010802)
        .count();
    assert_eq!(nvme_count, 1);

    let nic_count = guest_devices
        .iter()
        .filter(|d| d.class_code == 0x020000)
        .count();
    assert_eq!(nic_count, 1);

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
