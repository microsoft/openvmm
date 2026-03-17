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
async fn pcie_root_emulation(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    const ECAM_SIZE: u64 = 256 * 1024 * 1024; // 256 MB
    const LOW_MMIO_SIZE: u64 = 64 * 1024 * 1024; // 64 MB
    const HIGH_MMIO_SIZE: u64 = 1024 * 1024 * 1024; // 1 GB

    let os_flavor = config.os_flavor();
    let (vm, agent) = config
        .modify_backend(|b| {
            b.with_custom_config(|c| {
                let low_mmio_start = c.memory.mmio_gaps[0].start();
                let high_mmio_end = c.memory.mmio_gaps[1].end();
                let pcie_low = MemoryRange::new(low_mmio_start - LOW_MMIO_SIZE..low_mmio_start);
                let pcie_high = MemoryRange::new(high_mmio_end..high_mmio_end + HIGH_MMIO_SIZE);
                let ecam_range = MemoryRange::new(pcie_low.start() - ECAM_SIZE..pcie_low.start());
                c.memory.pci_ecam_gaps.push(ecam_range);
                c.memory.pci_mmio_gaps.push(pcie_low);
                c.memory.pci_mmio_gaps.push(pcie_high);
                c.pcie_root_complexes.push(PcieRootComplexConfig {
                    index: 0,
                    name: "rc0".into(),
                    segment: 0,
                    start_bus: 0,
                    end_bus: 255,
                    ecam_range,
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
        .filter(|d| d.vendor_id == 0x1414 && d.device_id == 0xc030 && d.class_code == 0x060400)
        .count();

    assert_eq!(root_port_count, 4);

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Test PCIe hotplug: hot-add a device to a hotplug-capable port, verify the
/// guest sees it, then hot-remove it and verify it's gone.
///
/// Requires `CONFIG_HOTPLUG_PCI_PCIE=y` in the test kernel.
#[openvmm_test(linux_direct_x64)]
async fn pcie_hotplug(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    const ECAM_SIZE: u64 = 256 * 1024 * 1024;
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
                let ecam_range = MemoryRange::new(pcie_low.start() - ECAM_SIZE..pcie_low.start());
                c.memory.pci_ecam_gaps.push(ecam_range);
                c.memory.pci_mmio_gaps.push(pcie_low);
                c.memory.pci_mmio_gaps.push(pcie_high);
                c.pcie_root_complexes.push(PcieRootComplexConfig {
                    index: 0,
                    name: "rc0".into(),
                    segment: 0,
                    start_bus: 0,
                    end_bus: 255,
                    ecam_range,
                    low_mmio: pcie_low,
                    high_mmio: pcie_high,
                    ports: vec![
                        PcieRootPortConfig {
                            name: "rp0".into(),
                            hotplug: true,
                        },
                        PcieRootPortConfig {
                            name: "rp1".into(),
                            hotplug: false,
                        },
                    ],
                })
            })
        })
        .run()
        .await?;

    // Verify initial state: only root ports, no endpoints
    let initial_devices = parse_guest_pci_devices(os_flavor, &agent).await?;
    let initial_endpoints = initial_devices
        .iter()
        .filter(|d| d.class_code != 0x060400) // filter out PCI-to-PCI bridges (root ports)
        .count();
    tracing::info!(?initial_devices, "initial PCI devices");
    assert_eq!(initial_endpoints, 0, "expected no endpoints initially");

    // Hot-add an NVMe controller (no namespaces) to rp0
    let nvme_resource = vm_resource::Resource::new(nvme_resources::NvmeControllerHandle {
        subsystem_id: guid::Guid::ZERO,
        msix_count: 2,
        max_io_queues: 1,
        namespaces: vec![],
        requests: None,
    });
    vm.add_pcie_device("rp0".into(), nvme_resource).await?;

    // Wait for Linux to enumerate the device (poll with retries)
    let mut found = false;
    for attempt in 0..30 {
        let devices = parse_guest_pci_devices(os_flavor, &agent).await?;
        let endpoints = devices
            .iter()
            .filter(|d| d.class_code != 0x060400)
            .count();
        if endpoints >= 1 {
            tracing::info!(?devices, attempt, "device appeared after hotplug");
            found = true;
            break;
        }
        // Use guest-side sleep to avoid needing tokio on the host
        let sh = agent.unix_shell();
        cmd!(sh, "sleep 0.5").run().await?;
    }
    assert!(found, "expected NVMe endpoint to appear after hot-add");

    // Hot-remove the device
    vm.remove_pcie_device("rp0".into()).await?;

    // Wait for Linux to de-enumerate the device
    let mut removed = false;
    for attempt in 0..30 {
        let devices = parse_guest_pci_devices(os_flavor, &agent).await?;
        let endpoints = devices
            .iter()
            .filter(|d| d.class_code != 0x060400)
            .count();
        if endpoints == 0 {
            tracing::info!(attempt, "device removed after hot-remove");
            removed = true;
            break;
        }
        let sh = agent.unix_shell();
        cmd!(sh, "sleep 0.5").run().await?;
    }
    assert!(removed, "expected endpoint to disappear after hot-remove");

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
