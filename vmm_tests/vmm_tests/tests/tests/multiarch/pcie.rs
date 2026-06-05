// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// All of the PCIe tests on Linux direct have been marked as unstable due to
// what is presumed to be a bug in the guest kernel currently being used.
// TODO: remove the unstable designations once the kernel is updated.

use crate::multiarch::OsFlavor;
use crate::multiarch::cmd;
use anyhow::Context;
use guid::Guid;
use net_backend_resources::mac_address::MacAddress;
use pal_async::DefaultDriver;
use pal_async::timer::PolledTimer;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use petri_artifacts_vmm_test::artifacts::virtio_win::VIRTIO_WIN_DRIVERS;
use pipette_client::PipetteClient;
use std::fmt;
use std::time::Duration;
use vmm_test_macros::openvmm_test;
use vmm_test_macros::vmm_test_with;

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
    unstable_linux_direct_x64,
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
/// guest software in a topology with multiple segments. Uses 10
/// ports per root complex to exercise multi-function packing across
/// multiple PCI device slots.
#[openvmm_test(
    unstable_linux_direct_x64,
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
        .modify_backend(|b| b.with_pcie_root_topology(4, 1, 10))
        .run()
        .await?;

    let guest_devices = parse_guest_pci_devices(os_flavor, &agent).await?;
    tracing::info!(?guest_devices, "guest devices");

    let root_port_count = guest_devices
        .iter()
        .filter(|d| d.vendor_id == 0x1414 && d.device_id == 0xc030 && d.class_code == 0x060400)
        .count();

    assert_eq!(root_port_count, 40);

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Test PCIe switch enumeration when attached to both root
/// ports and the downstream switch ports of other switches.
#[openvmm_test(
    unstable_linux_direct_x64,
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
#[openvmm_test(unstable_linux_direct_x64)]
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
#[openvmm_test(
    unstable_linux_direct_x64,
    uefi_x64(vhd(windows_datacenter_core_2022_x64))
)]
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
#[openvmm_test(unstable_linux_direct_x64)]
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

/// Boot a guest through UEFI from an NVMe device on an emulated PCIe root port.
/// Validates that UEFI's driver stack correctly enumerates and uses the NVMe
/// device to load the guest OS.
#[openvmm_test(
    uefi_x64(vhd(alpine_3_23_x64)),
    uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    uefi_aarch64(vhd(alpine_3_23_aarch64)),
    uefi_aarch64(vhd(windows_11_enterprise_aarch64))
)]
async fn pcie_nvme_boot(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (vm, agent) = config
        .with_boot_device_type(petri::BootDeviceType::PcieNvme)
        .with_default_boot_always_attempt(true)
        .modify_backend(|b| b.with_pcie_root_topology(1, 1, 1))
        .run()
        .await?;

    // Verify the NVMe device is visible from guest
    let guest_devices = parse_guest_pci_devices(os_flavor, &agent).await?;
    tracing::info!(?guest_devices, "guest devices");

    let nvme_count = guest_devices
        .iter()
        .filter(|d| d.class_code == 0x010802)
        .count();
    assert!(nvme_count >= 1, "NVMe controller not visible in guest");

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Test SMMUv3 IOMMU emulation with a mixed topology:
///
/// - Root complex s0rc0 (segment 0): SMMU enabled, virtio-net + NVMe behind it
/// - Root complex s1rc0 (segment 1): no SMMU, virtio-net behind it
///
/// Verifies:
/// 1. Linux discovers the SMMUv3 (dmesg shows arm-smmu-v3 init)
/// 2. IORT ACPI table is present
/// 3. Devices behind the SMMU RC are in IOMMU groups
/// 4. Devices on both RCs enumerate and function (block I/O, network interfaces)
/// 5. DMA through SMMU works (NVMe I/O behind the SMMU)
#[openvmm_test(linux_direct_aarch64)]
async fn smmu_mixed_topology(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let (vm, agent) = config
        .modify_backend(|b| {
            b.with_pcie_root_topology(2, 1, 4) // 2 segments, 1 RC each, 4 ports each
                .with_smmu(&["s0rc0"]) // SMMU only on segment 0's RC
                .with_pcie_nvme("s0rc0rp0", PCIE_NVME_SUBSYSTEM_IDS[0])
                .with_virtio_nic("s0rc0rp1")
                .with_pcie_nvme("s1rc0rp0", PCIE_NVME_SUBSYSTEM_IDS[1])
                .with_virtio_nic("s1rc0rp1")
        })
        .run()
        .await?;

    let sh = agent.unix_shell();

    // 1. Verify SMMUv3 is discovered by Linux
    let dmesg = cmd!(sh, "dmesg").read().await?;
    tracing::info!(dmesg_len = dmesg.len(), "dmesg captured");

    let smmu_lines: Vec<&str> = dmesg
        .lines()
        .filter(|l| l.contains("smmu") || l.contains("SMMU") || l.contains("arm-smmu"))
        .collect();
    tracing::info!(?smmu_lines, "SMMU-related dmesg lines");
    assert!(
        dmesg.contains("arm-smmu-v3"),
        "Linux should discover the SMMUv3 in dmesg. SMMU lines:\n{}",
        smmu_lines.join("\n")
    );

    // 2. Verify IORT ACPI table is present
    let acpi_tables = cmd!(sh, "ls /sys/firmware/acpi/tables/").read().await?;
    assert!(
        acpi_tables.contains("IORT"),
        "IORT ACPI table should be present. Tables: {acpi_tables}"
    );

    // 3. Verify IOMMU groups exist (devices behind the SMMU RC)
    let iommu_groups = cmd!(sh, "ls /sys/kernel/iommu_groups/").read().await?;
    tracing::info!(%iommu_groups, "IOMMU groups");
    assert!(
        !iommu_groups.trim().is_empty(),
        "IOMMU groups should exist for devices behind the SMMU"
    );

    // 4. Verify all NVMe devices enumerate, have block devices, and DMA
    //    works through the SMMU (segment 0 / PCI domain 0000).
    verify_nvme_dma_on_segment(&sh, 2, "0000").await?;

    // 5. Verify virtio-net interfaces exist on both RCs
    verify_net_interface_count(&sh, 2).await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Test AMD IOMMU emulation with a mixed topology:
///
/// - Root complex s0rc0 (segment 0): IOMMU enabled, virtio-net + NVMe behind it
/// - Root complex s1rc0 (segment 1): no IOMMU, virtio-net behind it
///
/// Verifies:
/// 1. Linux discovers the AMD IOMMU (dmesg shows AMD-Vi init)
/// 2. IVRS ACPI table is present
/// 3. Devices behind the IOMMU RC are in IOMMU groups
/// 4. Devices on both RCs enumerate and function (block I/O, network interface)
/// 5. DMA through the IOMMU works (NVMe I/O behind the IOMMU)
///
/// Restricted to AMD-vendor hosts: the AMD IOMMU emulator's IVHD entries
/// surface a host-cpuid-derived AMD-Vi family/model that Linux's IOMMU
/// driver only accepts when the boot CPU also reports as AMD.
#[vmm_test_with(openvmm_amd(linux_direct_x64))]
async fn amd_iommu_mixed_topology(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let (vm, agent) = config
        .modify_backend(|b| {
            b.with_pcie_root_topology(2, 1, 4) // 2 segments, 1 RC each, 4 ports each
                .with_amd_iommu(&["s0rc0"]) // IOMMU only on segment 0's RC
                .with_pcie_nvme("s0rc0rp0", PCIE_NVME_SUBSYSTEM_IDS[0])
                .with_virtio_nic("s0rc0rp1")
                .with_pcie_nvme("s1rc0rp0", PCIE_NVME_SUBSYSTEM_IDS[1])
                .with_virtio_nic("s1rc0rp1")
        })
        .run()
        .await?;

    let sh = agent.unix_shell();

    // 1. Verify IOMMU is discovered by Linux
    let dmesg = cmd!(sh, "dmesg").read().await?;
    tracing::info!(dmesg_len = dmesg.len(), "dmesg captured");

    assert!(
        dmesg.contains("AMD-Vi") || dmesg.contains("AMD IOMMUv2") || dmesg.contains("AMD IOMMU"),
        "Linux should discover the AMD IOMMU in dmesg. dmesg excerpt:\n{}",
        dmesg
            .lines()
            .filter(|l| l.contains("IOMMU") || l.contains("iommu") || l.contains("AMD-Vi"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // 2. Verify IVRS ACPI table is present
    let acpi_tables = cmd!(sh, "ls /sys/firmware/acpi/tables/").read().await?;
    assert!(
        acpi_tables.contains("IVRS"),
        "IVRS ACPI table should be present. Tables: {acpi_tables}"
    );

    // 3. Verify IOMMU groups exist (devices behind the IOMMU RC)
    let iommu_groups = cmd!(sh, "ls /sys/kernel/iommu_groups/").read().await?;
    tracing::info!(%iommu_groups, "IOMMU groups");
    assert!(
        !iommu_groups.trim().is_empty(),
        "IOMMU groups should exist for devices behind the IOMMU"
    );

    // 4. Verify all NVMe devices enumerate, have block devices, and DMA
    //    works through the IOMMU (segment 0 / PCI domain 0000).
    verify_nvme_dma_on_segment(&sh, 2, "0000").await?;

    // 5. Verify virtio-net interfaces exist
    verify_net_interface_count(&sh, 2).await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Boot a guest with VMBus entirely disabled.
///
/// Uses PCIe NVMe for the boot disk, virtio-vsock for pipette communication,
/// and a second PCIe NVMe controller for the cidata agent disk. Validates
/// that the guest boots and pipette is reachable without any VMBus devices.
#[openvmm_test(uefi_x64(vhd(alpine_3_23_x64)))]
async fn boot_no_vmbus_pcie_nvme(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_no_vmbus()
        .with_boot_device_type(petri::BootDeviceType::PcieNvme)
        .with_default_boot_always_attempt(true)
        .modify_backend(|b| b.with_pcie_root_topology(1, 1, 3))
        .run()
        .await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Verify that NVMe block devices are visible in the guest and exercise DMA
/// on the device whose PCI path falls in the given domain (segment).
async fn verify_nvme_dma_on_segment(
    sh: &pipette_client::shell::UnixShell<'_>,
    expected_count: usize,
    pci_domain: &str,
) -> anyhow::Result<()> {
    let block_devs = cmd!(sh, "ls /sys/block/").read().await?;
    let nvme_devs: Vec<&str> = block_devs
        .split_whitespace()
        .filter(|d| d.starts_with("nvme"))
        .collect();
    assert_eq!(
        nvme_devs.len(),
        expected_count,
        "expected {expected_count} NVMe block devices, found {}: {block_devs}",
        nvme_devs.len(),
    );

    // Find the NVMe device on the target PCI domain and exercise DMA.
    let domain_prefix = format!("{pci_domain}:");
    let mut target = None;
    for dev in &nvme_devs {
        let pci_path = cmd!(sh, "readlink -f /sys/block/{dev}/device")
            .read()
            .await?;
        if pci_path
            .split('/')
            .any(|seg| seg.starts_with(&domain_prefix))
        {
            target = Some(*dev);
            break;
        }
    }
    let target =
        target.unwrap_or_else(|| panic!("no NVMe device found on PCI domain {pci_domain}"));

    tracing::info!(target, pci_domain, "exercising DMA on NVMe device");
    cmd!(
        sh,
        "dd if=/dev/urandom of=/dev/{target} bs=4096 count=16 oflag=direct"
    )
    .read()
    .await?;
    cmd!(
        sh,
        "dd if=/dev/{target} of=/dev/null bs=4096 count=16 iflag=direct"
    )
    .read()
    .await?;

    Ok(())
}

/// Assert that the guest has at least `min_count` non-loopback network
/// interfaces.
async fn verify_net_interface_count(
    sh: &pipette_client::shell::UnixShell<'_>,
    min_count: usize,
) -> anyhow::Result<()> {
    let net_devs = cmd!(sh, "ls /sys/class/net/").read().await?;
    let net_count = net_devs.split_whitespace().filter(|d| *d != "lo").count();
    tracing::info!(%net_devs, net_count, "network devices");
    assert!(
        net_count >= min_count,
        "expected at least {min_count} network interfaces, got {net_count}: {net_devs}"
    );
    Ok(())
}

/// Boot Windows with VMBus entirely disabled.
///
/// Uses a prepped Windows image with NetKVM pre-installed and pipette
/// configured for TCP transport. Boots from PCIe NVMe, uses virtio-net +
/// consomme for TCP pipette communication.
#[openvmm_test(uefi_x64(vhd(windows_datacenter_core_2022_x64_no_vmbus_prepped)))]
async fn boot_no_vmbus_windows(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_no_vmbus()
        .with_boot_device_type(petri::BootDeviceType::PcieNvme)
        .with_default_boot_always_attempt(true)
        .modify_backend(|b| {
            b.with_pcie_root_topology(1, 1, 3)
                .with_tcp_pipette_nic("s0rc0rp2")
        })
        .run()
        .await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Boot Windows with a virtio-net NIC on PCIe, install the NetKVM driver
/// online via pipette, and verify the NIC gets a DHCP address from consomme.
///
/// This validates that our virtio-net emulation works with the upstream
/// virtio-win NetKVM driver on Windows.
#[openvmm_test(uefi_x64(vhd(windows_datacenter_core_2022_x64))[VIRTIO_WIN_DRIVERS])]
async fn virtio_net_windows(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (virtio_win,): (petri::ResolvedArtifact<VIRTIO_WIN_DRIVERS>,),
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    let driver_dir = virtio_win.get().join("NetKVM/2k22/amd64");

    let (vm, agent) = config
        .modify_backend(|b| {
            b.with_pcie_root_topology(1, 1, 1)
                .with_virtio_nic("s0rc0rp0")
        })
        .run()
        .await?;

    let sh = agent.windows_shell();

    // Create the driver directory in the guest
    cmd!(sh, "cmd.exe /c mkdir C:\\drivers").run().await?;

    // Push driver files into the guest
    let driver_files = [
        "netkvm.cat",
        "netkvm.inf",
        "netkvm.sys",
        "netkvmco.exe",
        "netkvmp.exe",
    ];
    for filename in &driver_files {
        let local_path = driver_dir.join(filename);
        let file = fs_err::File::open(&local_path)?;
        let guest_path = format!("C:/drivers/{filename}");
        agent
            .write_file(&guest_path, futures::io::AllowStdIo::new(file))
            .await
            .with_context(|| format!("failed to write {guest_path}"))?;
    }

    // Install the driver
    let output = cmd!(sh, "pnputil.exe /add-driver C:/drivers/netkvm.inf /install")
        .read()
        .await?;
    tracing::info!(%output, "pnputil output");

    // Wait for the NIC to get a DHCP address from consomme.
    // Consomme assigns 10.0.0.2 to the client.
    let mut timer = PolledTimer::new(&driver);
    let mut found = false;
    for attempt in 0..30 {
        let ipconfig = cmd!(sh, "ipconfig").read().await?;
        if ipconfig.contains("10.0.0.2") {
            tracing::info!(attempt, "virtio-net NIC got DHCP address");
            found = true;
            break;
        }
        tracing::debug!(attempt, "waiting for DHCP address...");
        timer.sleep(Duration::from_secs(2)).await;
    }
    assert!(
        found,
        "virtio-net NIC did not get a DHCP address (expected 10.0.0.2)"
    );

    // Verify we can ping the gateway
    let ping_output = cmd!(sh, "ping -n 1 10.0.0.1").read().await?;
    tracing::info!(%ping_output, "ping output");
    assert!(
        ping_output.contains("Reply from 10.0.0.1"),
        "ping to consomme gateway failed: {ping_output}"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
