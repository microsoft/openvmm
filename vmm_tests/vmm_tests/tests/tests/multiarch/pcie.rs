// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::multiarch::OsFlavor;
use crate::multiarch::cmd;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use pipette_client::PipetteClient;
use vmm_test_macros::openvmm_test;
use hvlite_defs::config::PcieRootComplexConfig;
use hvlite_defs::config::PcieRootPortConfig;

struct ParsedPciDevice {
    vendor_id: u16,
    _device_id: u16,
    class_code: u16,
}

async fn parse_guest_pci_devices(os_flavor: OsFlavor, agent: &PipetteClient) -> anyhow::Result<Vec<ParsedPciDevice>> {
    let mut devs = vec![];
    match os_flavor {
        OsFlavor::Linux => {
            let sh = agent.unix_shell();
            let output = cmd!(sh, "lspci -v -mm -n").read().await?.to_string();
            let lines = output.as_str().lines();

            let mut temp_ven: Option<u16> = None;
            let mut temp_dev: Option<u16> = None;
            let mut temp_class: Option<u16> = None;
            for line in lines {
                match line.split(":").collect::<Vec<&str>>().as_slice() {
                    ["Vendor", v] => temp_ven = Some(u16::from_str_radix(v.trim(), 16)?),
                    ["Device", d] => temp_dev = Some(u16::from_str_radix(d.trim(), 16)?),
                    ["Class", c] => temp_class = Some(u16::from_str_radix(c.trim(), 16)?),
                    _ => ()
                }

                if let (Some(v), Some(d), Some(c)) = (temp_ven, temp_dev, temp_class) {
                    devs.push(ParsedPciDevice{
                        vendor_id: v,
                        _device_id: d,
                        class_code: c,
                    });
                    temp_ven = None;
                    temp_dev = None;
                    temp_class = None;
                }
            }
        }
        OsFlavor::Windows => {
            let sh = agent.windows_shell();
            let output = cmd!(sh, "pnputil.exe /enum-devices /bus PCI /connected /properties")
                .read()
                .await?
                .to_string();

            let lines = output.as_str().lines();
            let mut parsing_hwids = false;
            for line in lines {
                if parsing_hwids {
                    // Find one matching PCI\VEN_XXXX&DEV_YYYY&CC_ZZZZ
                    let tok = line.trim().split("_").collect::<Vec<&str>>();
                    if tok.len() == 4 && tok[0].ends_with("VEN") && tok[1].ends_with("DEV") && tok[2].ends_with("CC") {
                        let v = u16::from_str_radix(&tok[1][..4], 16)?;
                        let d = u16::from_str_radix(&tok[2][..4], 16)?;
                        let c = u16::from_str_radix(&tok[3][..4], 16)?;
                        devs.push(ParsedPciDevice {
                            vendor_id: v,
                            _device_id: d,
                            class_code: c,
                        });
                        parsing_hwids = false;
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
    uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    uefi_x64(vhd(ubuntu_2404_server_x64)),
    uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    uefi_aarch64(vhd(ubuntu_2404_server_aarch64))
)]
async fn pcie_root_emulation(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (vm, agent) = config
        .modify_backend(|b| {
            b.with_custom_config(|c| {
                c.pcie_root_complexes.push(PcieRootComplexConfig {
                    index: 0,
                    name: "rc0".into(),
                    segment: 0,
                    start_bus: 0,
                    end_bus: 255,
                    low_mmio_size: 1024 * 1024,
                    high_mmio_size: 1024 * 1024 * 1024,
                    ports: vec![
                        PcieRootPortConfig {
                            name: "rp0".into(),
                        },
                        PcieRootPortConfig {
                            name: "rp1".into(),
                        },
                    ],
                })
            })
        })
        .run()
        .await?;

    let guest_devices = parse_guest_pci_devices(os_flavor, &agent).await?;
    assert_eq!(guest_devices.len(), 2);

    for dev in guest_devices {
        assert_eq!(dev.vendor_id, 0x1414);
        assert_eq!(dev.class_code, 0x0604);
    }

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
