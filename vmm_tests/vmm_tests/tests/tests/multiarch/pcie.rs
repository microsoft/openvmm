// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::multiarch::OsFlavor;
use crate::multiarch::cmd;
use guid::Guid;
use net_backend_resources::mac_address::MacAddress;
use pal_async::DefaultDriver;
use pal_async::timer::PolledTimer;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use pipette_client::PipetteClient;
use std::fmt;
use std::time::Duration;
use vmm_test_macros::openvmm_test;

/// List of MAC addresses for tests to use.
const PCIE_NIC_MAC_ADDRESSES: [MacAddress; 2] = [
    MacAddress::new([0x00, 0x15, 0x5D, 0x12, 0x12, 0x12]),
    MacAddress::new([0x00, 0x15, 0x5D, 0x12, 0x12, 0x13]),
];

/// List of NVMe Subsystem IDs for tests to use.
const PCIE_NVME_SUBSYSTEM_IDS: [Guid; 2] = [
    guid::guid!("55bfb22d-3f6c-4d5a-8ed8-d779dbdae6b8"),
    guid::guid!("6e4fbff0-eefc-4982-9e09-faf2f185701e"),
];

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

                // Device may disappear between ls and cat (e.g., during hotplug
                // removal), so skip devices whose sysfs files can't be read.
                let Ok(vendor_output) = cmd!(sh, "cat {device_sysfs_path}/vendor").read().await
                else {
                    continue;
                };
                let vendor_output = vendor_output.trim();
                let Ok(vendor_id) = u16::from_str_radix(
                    vendor_output.strip_prefix("0x").unwrap_or(vendor_output),
                    16,
                ) else {
                    continue;
                };

                let Ok(device_output) = cmd!(sh, "cat {device_sysfs_path}/device").read().await
                else {
                    continue;
                };
                let device_output = device_output.trim();
                let Ok(device_id) = u16::from_str_radix(
                    device_output.strip_prefix("0x").unwrap_or(device_output),
                    16,
                ) else {
                    continue;
                };

                let Ok(class_output) = cmd!(sh, "cat {device_sysfs_path}/class").read().await
                else {
                    continue;
                };
                let class_output = class_output.trim();
                let Ok(class_code) = u32::from_str_radix(
                    class_output.strip_prefix("0x").unwrap_or(class_output),
                    16,
                ) else {
                    continue;
                };

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
                // Reset state when we hit a new DEVPKEY section, even if we
                // were still looking for hardware IDs.
                if line.contains("DEVPKEY_Device_HardwareIds") {
                    parsing_hwids = true;
                    continue;
                } else if line.contains("DEVPKEY") {
                    parsing_hwids = false;
                    continue;
                }

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
                            if let (Ok(vendor_id), Ok(device_id), Ok(class_code)) = (
                                u16::from_str_radix(&tok1[..4], 16),
                                u16::from_str_radix(&tok2[..4], 16),
                                u32::from_str_radix(&tok3[..6], 16),
                            ) {
                                devs.push(ParsedPciDevice {
                                    vendor_id,
                                    device_id,
                                    class_code,
                                });
                            }
                            parsing_hwids = false;
                        }
                    }
                }
            }
        }
        _ => unreachable!(),
    }

    Ok(devs)
}

/// Test PCIe root complex discovery and root port enumeration by
/// guest software in a single segment topology.
#[openvmm_test(
    linux_direct_x64,
    uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    uefi_x64(vhd(ubuntu_2404_server_x64)),
    uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    uefi_aarch64(vhd(ubuntu_2404_server_aarch64))
)]
async fn pcie_root_emulation_single_segment(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (vm, agent) = config
        .modify_backend(|b| b.with_pcie_root_topology(1, 4, 4))
        .run()
        .await?;

    let guest_devices = parse_guest_pci_devices(os_flavor, &agent).await?;
    tracing::info!(?guest_devices, "guest devices");

    let root_port_count = guest_devices
        .iter()
        .filter(|d| d.vendor_id == 0x1414 && d.device_id == 0xc030 && d.class_code == 0x060400)
        .count();

    assert_eq!(root_port_count, 16);

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Test PCIe root complex discovery and root port enumeration by
/// guest software in a topology with multiple segments.
#[openvmm_test(
    linux_direct_x64,
    uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    uefi_x64(vhd(ubuntu_2404_server_x64)),
    uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    uefi_aarch64(vhd(ubuntu_2404_server_aarch64))
)]
async fn pcie_root_emulation_multi_segment(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (vm, agent) = config
        .modify_backend(|b| b.with_pcie_root_topology(4, 1, 8))
        .run()
        .await?;

    let guest_devices = parse_guest_pci_devices(os_flavor, &agent).await?;
    tracing::info!(?guest_devices, "guest devices");

    let root_port_count = guest_devices
        .iter()
        .filter(|d| d.vendor_id == 0x1414 && d.device_id == 0xc030 && d.class_code == 0x060400)
        .count();

    assert_eq!(root_port_count, 32);

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Test PCIe switch enumeration when attached to both root
/// ports and the downstream switch ports of other switches.
#[openvmm_test(
    linux_direct_x64,
    uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    uefi_x64(vhd(ubuntu_2404_server_x64)),
    uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    uefi_aarch64(vhd(ubuntu_2404_server_aarch64))
)]
async fn pcie_switches(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (vm, agent) = config
        .modify_backend(|b| {
            b.with_pcie_root_topology(1, 1, 4)
                .with_pcie_switch("s0rc0rp0", "sw0", 2, false)
                .with_pcie_switch("s0rc0rp1", "sw1", 2, false)
                .with_pcie_switch("sw1-downstream-1", "sw2", 2, false)
        })
        .run()
        .await?;

    let guest_devices = parse_guest_pci_devices(os_flavor, &agent).await?;
    tracing::info!(?guest_devices, "guest devices");

    let upstream_switch_port_count = guest_devices
        .iter()
        .filter(|d| d.vendor_id == 0x1414 && d.device_id == 0xc031 && d.class_code == 0x060400)
        .count();
    assert_eq!(upstream_switch_port_count, 3);

    let downstream_switch_port_count = guest_devices
        .iter()
        .filter(|d| d.vendor_id == 0x1414 && d.device_id == 0xc032 && d.class_code == 0x060400)
        .count();
    assert_eq!(downstream_switch_port_count, 6);

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Test PCIe device enumeration using a selection of device
/// emulators, when attached to both root ports and downstream
/// switch ports.
///
/// NOTE: This test relies on device specific software (drivers,
/// tooling) within the guest OS to perform the validation.
#[openvmm_test(linux_direct_x64)]
async fn pcie_devices(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (vm, agent) = config
        .modify_backend(|b| {
            b.with_pcie_root_topology(1, 1, 8)
                .with_pcie_nvme("s0rc0rp0", PCIE_NVME_SUBSYSTEM_IDS[0])
                .with_pcie_nic("s0rc0rp1", PCIE_NIC_MAC_ADDRESSES[0])
                .with_pcie_switch("s0rc0rp3", "sw0", 2, false)
                .with_pcie_nvme("sw0-downstream-0", PCIE_NVME_SUBSYSTEM_IDS[1])
                .with_pcie_nic("sw0-downstream-1", PCIE_NIC_MAC_ADDRESSES[1])
        })
        .run()
        .await?;

    let guest_devices = parse_guest_pci_devices(os_flavor, &agent).await?;
    tracing::info!(?guest_devices, "guest devices");

    // Confirm the NVMe controllers enumerate at the PCI level
    let nvme_count = guest_devices
        .iter()
        .filter(|d| d.class_code == 0x010802)
        .count();
    assert_eq!(nvme_count, 2);

    // Confirm the MANA device enumerates at the PCI level
    let nic_count = guest_devices
        .iter()
        .filter(|d| d.class_code == 0x020000)
        .count();
    assert_eq!(nic_count, 2);

    let sh = agent.unix_shell();

    // Confirm the NVMe controllers show up as block devices
    let nsid_output = cmd!(sh, "cat /sys/block/nvme0n1/nsid").read().await?;
    assert_eq!(nsid_output, "1");
    let nsid_output = cmd!(sh, "cat /sys/block/nvme1n1/nsid").read().await?;
    assert_eq!(nsid_output, "1");

    // Confirm the MANA devices show up as ethernet adapters with
    // the right MAC addresses
    let mut mac_output: [String; 2] = [
        cmd!(sh, "cat /sys/class/net/eth0/address").read().await?,
        cmd!(sh, "cat /sys/class/net/eth1/address").read().await?,
    ];
    mac_output.sort();
    assert_eq!(mac_output[0], "00:15:5d:12:12:12");
    assert_eq!(mac_output[1], "00:15:5d:12:12:13");

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Test PCIe hotplug: hot-add a device to a hotplug-capable port, verify the
/// guest sees it, then hot-remove it and verify it's gone.
#[openvmm_test(linux_direct_x64, uefi_x64(vhd(windows_datacenter_core_2022_x64)))]
async fn pcie_hotplug(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    _: (),
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (mut vm, agent) = config
        .modify_backend(|b| b.with_pcie_root_topology(1, 1, 2))
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

    // Hot-add an NVMe controller (no namespaces) to the first root port
    let nvme_resource = vm_resource::Resource::new(nvme_resources::NvmeControllerHandle {
        subsystem_id: PCIE_NVME_SUBSYSTEM_IDS[0],
        msix_count: 2,
        max_io_queues: 1,
        namespaces: vec![],
        requests: None,
    });
    vm.add_pcie_device("s0rc0rp0".into(), nvme_resource).await?;

    // Wait for the guest to enumerate the device (poll with retries)
    let mut timer = PolledTimer::new(&driver);
    let mut found = false;
    for attempt in 0..30 {
        let devices = parse_guest_pci_devices(os_flavor, &agent).await?;
        let endpoints = devices.iter().filter(|d| d.class_code != 0x060400).count();
        if endpoints >= 1 {
            tracing::info!(?devices, attempt, "device appeared after hotplug");
            found = true;
            break;
        }
        timer.sleep(Duration::from_millis(500)).await;
    }
    assert!(found, "expected NVMe endpoint to appear after hot-add");

    // Wait for the guest to fully process the add event before removing.
    timer.sleep(Duration::from_secs(5)).await;

    // Hot-remove the device
    vm.remove_pcie_device("s0rc0rp0".into()).await?;

    // Verify the device is gone. Both Linux (pciehp) and Windows (pci.sys)
    // process native PCIe hotplug surprise-removal through their respective
    // hotplug state machines within a few seconds.
    let mut removed = false;
    for attempt in 0..30 {
        let devices = parse_guest_pci_devices(os_flavor, &agent).await?;
        let endpoints = devices.iter().filter(|d| d.class_code != 0x060400).count();
        if endpoints == 0 {
            tracing::info!(attempt, "device removed after hot-remove");
            removed = true;
            break;
        }
        timer.sleep(Duration::from_millis(500)).await;
    }
    assert!(removed, "expected endpoint to disappear after hot-remove");

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Verify PCIe root complex state survives a save/restore cycle.
///
/// This test:
/// 1. Boots a VM with a PCIe root complex and 4 root ports
/// 2. Enumerates PCI devices visible to the guest
/// 3. Pulses save/restore (pause → save → restore → resume)
/// 4. Re-enumerates PCI devices and verifies they match
#[openvmm_test(linux_direct_x64)]
async fn pcie_save_restore(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (mut vm, agent) = config
        .modify_backend(|b| b.with_pcie_root_topology(1, 1, 4))
        .run()
        .await?;

    // Snapshot pre-save PCI topology from the guest
    let devices_before = parse_guest_pci_devices(os_flavor, &agent).await?;
    tracing::info!(?devices_before, "PCI devices before save/restore");

    let root_ports_before = devices_before
        .iter()
        .filter(|d| d.vendor_id == 0x1414 && d.device_id == 0xc030 && d.class_code == 0x060400)
        .count();
    assert_eq!(
        root_ports_before, 4,
        "expected 4 root ports before save/restore"
    );

    // Pulse save/restore — drop agent first (vsock won't survive)
    drop(agent);
    vm.backend().verify_save_restore().await?;

    // Reconnect to the guest
    let agent = vm.backend().wait_for_agent(false).await?;

    // Re-enumerate and compare
    let devices_after = parse_guest_pci_devices(os_flavor, &agent).await?;
    tracing::info!(?devices_after, "PCI devices after save/restore");

    let root_ports_after = devices_after
        .iter()
        .filter(|d| d.vendor_id == 0x1414 && d.device_id == 0xc030 && d.class_code == 0x060400)
        .count();
    assert_eq!(
        root_ports_after, 4,
        "expected 4 root ports after save/restore"
    );

    // Verify total device count is unchanged (no devices lost or duplicated)
    assert_eq!(
        devices_before.len(),
        devices_after.len(),
        "PCI device count changed across save/restore"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
