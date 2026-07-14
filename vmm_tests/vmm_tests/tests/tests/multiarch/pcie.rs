// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

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
/// guest software in a topology with multiple segments. Uses 10
/// ports per root complex to exercise multi-function packing across
/// multiple PCI device slots.
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
#[openvmm_test(unstable(
    reason = "PCIe save/restore test is flaky on linux-direct in CI; root cause unknown",
    linux_direct_x64
))]
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
                // Set real ACS capability bits on root ports so Linux creates
                // per-device IOMMU groups (SV + RR + CR + UF).
                .with_custom_config(|c| {
                    for rc in &mut c.pcie_root_complexes {
                        for port in &mut rc.ports {
                            port.acs_capabilities_supported = Some(0x5D);
                        }
                    }
                })
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

    // 3–5. Common IOMMU validation: IOMMU groups, NVMe DMA, net, no faults.
    verify_iommu_mixed_topology(&sh, |l| l.contains("arm-smmu-v3") && l.contains("event"), 2)
        .await?;

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
///
/// Runs under both Linux direct boot and UEFI (Ubuntu VHD) to verify the
/// IVRS ACPI table is threaded through the UEFI firmware as well.
#[vmm_test_with(
    openvmm,
    amd,
    configs(linux_direct_x64, uefi_x64(vhd(ubuntu_2404_server_x64)))
)]
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
                // Set real ACS capability bits on root ports so Linux creates
                // per-device IOMMU groups (SV + RR + CR + UF).
                .with_custom_config(|c| {
                    for rc in &mut c.pcie_root_complexes {
                        for port in &mut rc.ports {
                            port.acs_capabilities_supported = Some(0x5D);
                        }
                    }
                })
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

    // 3b. Verify interrupt remapping is active for IOAPIC interrupts.
    verify_ioapic_interrupt_remapping(&sh, &dmesg, "AMD-Vi", |l| {
        let l = l.to_ascii_lowercase();
        l.contains("remap") || l.contains("amd-vi")
    })
    .await?;

    // 3–5. Common IOMMU validation: IOMMU groups, NVMe DMA, net, no faults.
    verify_iommu_mixed_topology(
        &sh,
        |l| l.contains("AMD-Vi: Event") || l.contains("IO_PAGE_FAULT"),
        2,
    )
    .await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Test Intel VT-d IOMMU emulation across a multi-segment topology:
///
/// - Root complex s0rc0 (segment 0): VT-d IOMMU enabled, NVMe + virtio-net
/// - Root complex s1rc0 (segment 1): VT-d IOMMU enabled, NVMe + virtio-net
///
/// Every root complex has its own VT-d unit. Unlike AMD-Vi, Intel VT-d cannot
/// have a device on a segment with no VT-d unit: enabling VT-d forces global
/// interrupt remapping (x2APIC), under which Linux can't allocate MSIs for a
/// device outside every DRHD's scope (so OpenVMM rejects that configuration).
///
/// Verifies:
/// 1. Linux discovers the Intel IOMMU (dmesg shows DMAR/Intel IOMMU init)
/// 2. DMAR ACPI table is present
/// 3. Devices behind the IOMMU RC are in IOMMU groups
/// 4. Devices on both RCs enumerate and function (block I/O, network interface)
/// 5. DMA through the IOMMU works (NVMe I/O behind the IOMMU)
///
/// Runs under both Linux direct boot and UEFI (Ubuntu VHD) to verify the
/// DMAR ACPI table is threaded through the UEFI firmware as well.
#[vmm_test_with(
    openvmm,
    intel,
    configs(linux_direct_x64, uefi_x64(vhd(ubuntu_2404_server_x64)))
)]
async fn intel_vtd_multi_segment(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let (vm, agent) = config
        .modify_backend(|b| {
            b.with_pcie_root_topology(2, 1, 4) // 2 segments, 1 RC each, 4 ports each
                .with_intel_vtd(&["s0rc0", "s1rc0"]) // VT-d on every RC
                .with_pcie_nvme("s0rc0rp0", PCIE_NVME_SUBSYSTEM_IDS[0])
                .with_virtio_nic("s0rc0rp1")
                .with_pcie_nvme("s1rc0rp0", PCIE_NVME_SUBSYSTEM_IDS[1])
                .with_virtio_nic("s1rc0rp1")
                // Linux's Intel IOMMU driver is off by default unless the
                // kernel was built with CONFIG_INTEL_IOMMU_DEFAULT_ON.
                // Also set real ACS capability bits on root ports so Linux
                // creates per-device IOMMU groups (SV + RR + CR + UF).
                .with_custom_config(|c| {
                    if let openvmm_defs::config::LoadMode::Linux { cmdline, .. } = &mut c.load_mode
                    {
                        cmdline.push_str(" intel_iommu=on");
                    }
                    for rc in &mut c.pcie_root_complexes {
                        for port in &mut rc.ports {
                            port.acs_capabilities_supported = Some(0x5D);
                        }
                    }
                })
        })
        .run()
        .await?;

    let sh = agent.unix_shell();

    // 1. Verify Intel IOMMU is discovered by Linux
    let dmesg = cmd!(sh, "dmesg").read().await?;
    tracing::info!(dmesg_len = dmesg.len(), "dmesg captured");

    assert!(
        dmesg.contains("DMAR") || dmesg.contains("Intel IOMMU") || dmesg.contains("intel-iommu"),
        "Linux should discover the Intel IOMMU in dmesg. dmesg excerpt:\n{}",
        dmesg
            .lines()
            .filter(|l| {
                l.contains("IOMMU")
                    || l.contains("iommu")
                    || l.contains("DMAR")
                    || l.contains("dmar")
            })
            .collect::<Vec<_>>()
            .join("\n")
    );

    // 2. Verify DMAR ACPI table is present
    let acpi_tables = cmd!(sh, "ls /sys/firmware/acpi/tables/").read().await?;
    assert!(
        acpi_tables.contains("DMAR"),
        "DMAR ACPI table should be present. Tables: {acpi_tables}"
    );

    // 3b. Verify interrupt remapping is active for IOAPIC interrupts.
    verify_ioapic_interrupt_remapping(&sh, &dmesg, "Intel VT-d", |l| {
        let l = l.to_ascii_lowercase();
        l.contains("remap") || l.contains("dmar") || l.contains("intel-iommu")
    })
    .await?;

    // 3–6. Common IOMMU validation: IOMMU groups, NVMe DMA, net, no faults.
    verify_iommu_mixed_topology(
        &sh,
        |l| l.contains("DMAR: [DMA") || l.contains("DMAR: DRHD: handling fault"),
        2,
    )
    .await?;

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

/// Common IOMMU validation for the mixed-topology tests.
///
/// After the platform-specific IOMMU discovery and ACPI table checks, this
/// function verifies that DMA remapping is actually working:
///
/// 1. At least `min_iommu_groups` IOMMU groups exist (ensures all devices
///    behind the IOMMU are scoped correctly, not just one).
/// 2. NVMe DMA works on the IOMMU-backed segment.
/// 3. Network interfaces are present on both segments.
/// 4. No IOMMU faults in dmesg (using the caller-provided `fault_filter`).
///    This is checked last, after exercising DMA/MSI, so that runtime faults
///    triggered by the actual translation and interrupt-remapping paths are
///    caught in addition to any boot-time faults.
async fn verify_iommu_mixed_topology(
    sh: &pipette_client::shell::UnixShell<'_>,
    fault_filter: impl Fn(&str) -> bool,
    min_iommu_groups: usize,
) -> anyhow::Result<()> {
    // Verify IOMMU groups cover all devices behind the IOMMU RC.
    let iommu_groups = cmd!(sh, "ls /sys/kernel/iommu_groups/").read().await?;
    let group_count = iommu_groups.split_whitespace().count();
    tracing::info!(%iommu_groups, group_count, "IOMMU groups");
    assert!(
        group_count >= min_iommu_groups,
        "expected at least {min_iommu_groups} IOMMU groups, got {group_count}"
    );

    // Verify NVMe DMA works through the IOMMU (segment 0 / PCI domain 0000).
    verify_nvme_dma_on_segment(sh, 2, "0000").await?;

    // Verify network interfaces exist on both RCs.
    verify_net_interface_count(sh, 2).await?;

    // Verify no IOMMU faults — faults indicate broken device scope, missing
    // page table mappings, or devices not covered by the IOMMU. Re-read dmesg
    // here (after the DMA exercise above) so faults from the actual
    // translation/interrupt-remapping path are caught, not just boot-time ones.
    let dmesg = cmd!(sh, "dmesg").read().await?;
    let faults: Vec<&str> = dmesg.lines().filter(|l| fault_filter(l)).collect();
    assert!(
        faults.is_empty(),
        "IOMMU faults detected — DMA remapping is not working correctly:\n{}",
        faults.join("\n")
    );

    Ok(())
}

/// Verify that IOAPIC interrupts are routed through interrupt remapping.
///
/// Linux reports this path as `IR-IO-APIC` in `/proc/interrupts`. To prove the
/// route is live, this captures the serial IRQ count, generates serial output,
/// and confirms the count increases.
async fn verify_ioapic_interrupt_remapping(
    sh: &pipette_client::shell::UnixShell<'_>,
    dmesg: &str,
    iommu: &str,
    ir_dmesg_filter: impl Fn(&str) -> bool,
) -> anyhow::Result<()> {
    tracing::info!(
        iommu,
        ir_dmesg = %dmesg
            .lines()
            .filter(|l| ir_dmesg_filter(l))
            .collect::<Vec<_>>()
            .join("\n"),
        "interrupt remapping dmesg lines"
    );

    let interrupts = cmd!(sh, "cat /proc/interrupts").read().await?;
    tracing::info!(%interrupts, "/proc/interrupts");

    let serial_irq = interrupts
        .lines()
        .find(|l| l.contains("ttyS0"))
        .context("serial port IRQ (ttyS0) not present in /proc/interrupts")?;
    assert!(
        serial_irq.contains("IR-IO-APIC"),
        "serial IRQ should route through the IR-IO-APIC chip once interrupt \
         remapping is enabled, got: {serial_irq}"
    );

    let count_before = sum_irq_count(serial_irq);
    cmd!(
        sh,
        "sh -c 'for i in $(seq 1 100); do echo ir-remap-test > /dev/ttyS0; done'"
    )
    .run()
    .await?;
    let interrupts_after = cmd!(sh, "cat /proc/interrupts").read().await?;
    let serial_irq_after = interrupts_after
        .lines()
        .find(|l| l.contains("ttyS0"))
        .context("serial port IRQ (ttyS0) disappeared from /proc/interrupts")?;
    let count_after = sum_irq_count(serial_irq_after);
    tracing::info!(count_before, count_after, "serial IOAPIC interrupt counts");
    assert!(
        count_after > count_before,
        "serial (IOAPIC) interrupt count should increase after generating \
         serial traffic with interrupt remapping enabled: \
         before={count_before} after={count_after}"
    );

    Ok(())
}

/// Sum the per-CPU interrupt counts from a `/proc/interrupts` line.
///
/// A line looks like `" 4:   42    0   IR-IO-APIC   4-edge   ttyS0"`: the
/// leading token is the IRQ label and the trailing tokens are the chip and
/// device name, so only the numeric per-CPU columns in between are summed.
fn sum_irq_count(line: &str) -> u64 {
    line.split_whitespace()
        .skip(1)
        .map_while(|tok| tok.parse::<u64>().ok())
        .sum()
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

    // Verify we can ping the gateway. The first ping immediately after the NIC
    // gets its DHCP lease can time out while the guest network stack finishes
    // coming up (e.g. resolving the gateway's ARP entry), so retry until we get
    // a reply. `ping` exits non-zero when a request times out, so ignore the
    // status and inspect the output instead of failing the command outright.
    let mut pinged = false;
    for attempt in 0..5 {
        let ping_output = cmd!(sh, "ping -n 1 10.0.0.1")
            .ignore_status()
            .read()
            .await?;
        if ping_output.contains("Reply from 10.0.0.1") {
            tracing::info!(attempt, %ping_output, "ping output");
            pinged = true;
            break;
        }
        tracing::debug!(attempt, %ping_output, "waiting for gateway to respond to ping...");
        timer.sleep(Duration::from_secs(2)).await;
    }
    assert!(
        pinged,
        "ping to consomme gateway failed (no reply from 10.0.0.1)"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
