// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::multiarch::OsFlavor;
use crate::multiarch::cmd;
use memory_range::MemoryRange;
use openvmm_defs::config::PcieRootComplexConfig;
use openvmm_defs::config::PcieRootPortConfig;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use pipette_client::PipetteClient;
use std::fmt;
use vmm_test_macros::openvmm_test;

struct ParsedPciDevice {
    vendor_id: u16,
    device_id: u16,
    class_code: u16,
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
            let sh = agent.unix_shell();
            let output = cmd!(sh, "lspci -v -mm -n").read().await?;
            let lines = output.as_str().lines();

            let mut temp_ven: Option<u16> = None;
            let mut temp_dev: Option<u16> = None;
            let mut temp_class: Option<u16> = None;
            for line in lines {
                match line.split_once(":") {
                    Some(("Vendor", v)) => temp_ven = Some(u16::from_str_radix(v.trim(), 16)?),
                    Some(("Device", d)) => temp_dev = Some(u16::from_str_radix(d.trim(), 16)?),
                    Some(("Class", c)) => temp_class = Some(u16::from_str_radix(c.trim(), 16)?),
                    _ => (),
                }

                if let (Some(v), Some(d), Some(c)) = (temp_ven, temp_dev, temp_class) {
                    devs.push(ParsedPciDevice {
                        vendor_id: v,
                        device_id: d,
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
                    // Find one matching PCI\VEN_XXXX&DEV_YYYY&CC_ZZZZ
                    let mut toks = line.trim().split('_');
                    if let (Some(tok0), Some(tok1), Some(tok2), Some(tok3)) =
                        (toks.next(), toks.next(), toks.next(), toks.next())
                    {
                        if tok0.ends_with("VEN") && tok1.ends_with("DEV") && tok2.ends_with("CC") {
                            let v = u16::from_str_radix(&tok1[..4], 16)?;
                            let d = u16::from_str_radix(&tok2[..4], 16)?;
                            let c = u16::from_str_radix(&tok3[..4], 16)?;
                            devs.push(ParsedPciDevice {
                                vendor_id: v,
                                device_id: d,
                                class_code: c,
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
    uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    uefi_x64(vhd(ubuntu_2404_server_x64)),
    uefi_aarch64(vhd(windows_11_enterprise_aarch64))
    // uefi_aarch64(vhd(ubuntu_2404_server_aarch64))
)]
async fn pcie_root_emulation(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    const LOW_MMIO_SIZE: u64 = 64 * 1024 * 1024;
    const HIGH_MMIO_SIZE: u64 = 1024 * 1024 * 1024;

    let os_flavor = config.os_flavor();
    let (vm, agent) = config
        .modify_backend(|b| {
            b.with_custom_config(|c| {
                let low_mmio_start = c.memory.mmio_gaps[0].start();
                let high_mmio_end = c.memory.mmio_gaps[1].end();
                let pcie_low = MemoryRange::new(low_mmio_start - LOW_MMIO_SIZE..low_mmio_start);
                let pcie_high = MemoryRange::new(high_mmio_end..high_mmio_end + HIGH_MMIO_SIZE);
                c.memory.device_reserved_gaps.push(pcie_low);
                c.memory.device_reserved_gaps.push(pcie_high);
                c.pcie_root_complexes.push(PcieRootComplexConfig {
                    index: 0,
                    name: "rc0".into(),
                    segment: 0,
                    start_bus: 0,
                    end_bus: 255,
                    low_mmio: pcie_low,
                    high_mmio: pcie_high,
                    ports: vec![
                        PcieRootPortConfig {
                            name: "rp0".into(),
                            hotplug: false,
                        },
                        PcieRootPortConfig {
                            name: "rp1".into(),
                            hotplug: false,
                        },
                        PcieRootPortConfig {
                            name: "rp2".into(),
                            hotplug: false,
                        },
                        PcieRootPortConfig {
                            name: "rp3".into(),
                            hotplug: false,
                        },
                    ],
                })
            })
        })
        .run()
        .await?;

    let guest_devices = parse_guest_pci_devices(os_flavor, &agent).await?;
    tracing::info!(?guest_devices, "guest devices");

    let root_port_count = guest_devices
        .iter()
        .filter(|d| d.vendor_id == 0x1414 && d.device_id == 0xc030 && d.class_code == 0x0604)
        .count();

    assert_eq!(root_port_count, 4);

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
