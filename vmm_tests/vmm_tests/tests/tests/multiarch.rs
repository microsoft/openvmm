// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests that run on more than one architecture.

use anyhow::Context;
use futures::StreamExt;
use petri::MemoryConfig;
use petri::PetriHaltReason;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ProcessorTopology;
use petri::SIZE_1_GB;
use petri::ShutdownKind;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use petri_artifacts_common::tags::MachineArch;
use petri_artifacts_common::tags::OsFlavor;
#[cfg(target_os = "linux")]
use petri_artifacts_vmm_test::artifacts::OPENVMM_VHOST_NATIVE;
use vmm_test_macros::openvmm_test;
use vmm_test_macros::vmm_test;
use vmm_test_macros::vmm_test_with;

/// Tests for Hyper-V integration components.
mod ic;
// Memory Validation tests.
mod memstat;
/// Servicing tests.
mod openhcl_servicing;
/// PCIe emulation tests.
mod pcie;
/// Tests involving TPM functionality
mod tpm;
/// Tests for VLAN (802.1Q) support on virtual NICs.
mod vlan;
/// Tests of vmbus relay functionality.
mod vmbus_relay;
/// Tests involving VMGS functionality
mod vmgs;

/// Boot through the UEFI firmware, it will shut itself down after booting.
#[vmm_test_with(noagent(
    openvmm_uefi_x64(none),
    openvmm_openhcl_uefi_x64(none),
    openvmm_uefi_aarch64(none),
    hyperv_openhcl_uefi_aarch64(none),
    hyperv_openhcl_uefi_x64(none)
))]
async fn frontpage<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let vm = config.run_without_agent().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Basic boot test.
#[vmm_test(
    openvmm_linux_direct_x64,
    openvmm_linux_direct_aarch64,
    openvmm_openhcl_linux_direct_x64,
    openvmm_pcat_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_pcat_x64(vhd(ubuntu_2404_server_x64)),
    openvmm_pcat_x64(vhd(ubuntu_2504_server_x64)),
    openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_uefi_x64(vhd(ubuntu_2404_server_x64)),
    openvmm_uefi_x64(vhd(ubuntu_2504_server_x64)),
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2404_server_x64)),
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_pcat_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_openhcl_pcat_x64(vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2404_server_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    unstable_openvmm_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped)),
    // openvmm_openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[tdx](vhd(ubuntu_2504_server_x64))
)]
async fn boot<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let (vm, agent) = config.run().await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Basic boot test using virtio vsock instead of vmbus hvsocket.
/// N.B. Because this requires kernel support, it's only done for Linux direct boot since the test
///      kernel is guaranteed to include it.
#[vmm_test(openvmm_linux_direct_x64, openvmm_linux_direct_aarch64)]
async fn boot_virtio_vsock(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_virtio_vsock()
        .modify_backend(|b| b.with_pcie_root_topology(1, 1, 1))
        .run()
        .await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Boot with private anonymous memory instead of shared memory sections.
#[openvmm_test(
    linux_direct_x64,
    // TODO: add linux_direct_aarch64 (GH #1798)
)]
async fn boot_private_memory(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let (vm, agent) = config
        .modify_backend(|b| {
            b.with_custom_config(|c| {
                c.memory.private_memory = true;
            })
        })
        .run()
        .await?;

    agent.ping().await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Boot Linux with guest memory backed by explicit 2 MiB hugetlb pages.
#[cfg(target_os = "linux")]
#[openvmm_test(linux_direct_x64)]
#[openvmm_test(linux_direct_aarch64)]
async fn hugetlb_memory_boot(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    const RAM_BYTES: u64 = 1024 * 1024 * 1024;

    let required_pages = RAM_BYTES / petri::openvmm::HUGETLB_2MB_PAGE_SIZE;
    if !petri::openvmm::ensure_2mb_hugetlb_pages(required_pages)? {
        return Ok(());
    }

    let (vm, agent) = config
        .with_memory(MemoryConfig {
            startup_bytes: RAM_BYTES,
            ..Default::default()
        })
        .modify_backend(|b| b.with_hugepages(Some(petri::openvmm::HUGETLB_2MB_PAGE_SIZE)))
        .run()
        .await?;

    agent.ping().await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Basic boot test for images that require small amounts of ram, like alpine.
#[vmm_test(
    openvmm_uefi_x64(vhd(alpine_3_23_x64)),
    openvmm_openhcl_uefi_x64(vhd(alpine_3_23_x64)),
    hyperv_openhcl_uefi_x64(vhd(alpine_3_23_x64)),
    openvmm_uefi_aarch64(vhd(alpine_3_23_aarch64)),
    openvmm_openhcl_uefi_aarch64(vhd(alpine_3_23_aarch64)),
    hyperv_openhcl_uefi_aarch64(vhd(alpine_3_23_aarch64))
)]
async fn boot_small<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_memory(MemoryConfig {
            startup_bytes: SIZE_1_GB,
            ..Default::default()
        })
        .run()
        .await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Basic boot test without agent
#[vmm_test_with(noagent(
    openvmm_pcat_x64(vhd(freebsd_13_2_x64)),
    openvmm_pcat_x64(iso(freebsd_13_2_x64))
))]
async fn boot_no_agent<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let mut vm = config.run_without_agent().await?;
    vm.send_enlightened_shutdown(ShutdownKind::Shutdown).await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

// Basic vp "heavy" boot test with 16 VPs and 2 NUMA nodes.
#[vmm_test(
    openvmm_linux_direct_x64,
    openvmm_openhcl_linux_direct_x64,
    openvmm_pcat_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_pcat_x64(vhd(ubuntu_2504_server_x64)),
    openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_uefi_x64(vhd(ubuntu_2504_server_x64)),
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_pcat_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_openhcl_pcat_x64(vhd(ubuntu_2504_server_x64)),
    unstable_hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    unstable_openvmm_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped)),
    // openvmm_openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[tdx](vhd(ubuntu_2504_server_x64))
)]
async fn boot_heavy<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_processor_topology(ProcessorTopology::heavy())
        .run()
        .await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Basic boot test with a single VP.
#[vmm_test(
    unstable_openvmm_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped)),
    // openvmm_openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[tdx](vhd(ubuntu_2504_server_x64))
)]
async fn boot_single_proc<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_processor_topology(ProcessorTopology {
            vp_count: 1,
            ..Default::default()
        })
        .run()
        .await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

#[vmm_test_with(vpci(
    // TODO: virt_whp is missing VPCI LPI interrupt support, used by Windows (but not Linux)
    // openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    // TODO: Linux image is missing VPCI driver in its initrd
    // openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    // openvmm_uefi_x64(vhd(ubuntu_2504_server_x64))
))]
async fn boot_nvme<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_boot_device_type(petri::BootDeviceType::Nvme)
        .run()
        .await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Tests NVMe boot with OpenHCL VPCI relaying enabled.
#[vmm_test_with(vpci(
    // TODO: aarch64 support (WHP missing ARM64 VTL2 support)
    // openvmm_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    // openvmm_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    // TODO: Linux image is missing VPCI driver in its initrd
    // openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))
))]
async fn boot_nvme_vpci_relay<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_boot_device_type(petri::BootDeviceType::Nvme)
        .with_openhcl_command_line("OPENHCL_ENABLE_VPCI_RELAY=1")
        .with_vmbus_redirect(true)
        .run()
        .await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Validate we can reboot a VM and reconnect to pipette.
// TODO: Reenable openvmm guests that use the framebuffer once #74 is fixed.
#[vmm_test(
    openvmm_linux_direct_x64,
    openvmm_openhcl_linux_direct_x64,
    // openvmm_pcat_x64(vhd(windows_datacenter_core_2022_x64)),
    // openvmm_pcat_x64(vhd(ubuntu_2504_server_x64)),
    // openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    // openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    // openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    // openvmm_uefi_x64(vhd(ubuntu_2504_server_x64)),
    // openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    // openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_pcat_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_openhcl_pcat_x64(vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    unstable_openvmm_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped)),
    // openvmm_openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64[tdx](vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64))
)]
async fn reboot<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> Result<(), anyhow::Error> {
    let (mut vm, agent) = config.run().await?;
    agent.ping().await?;
    agent.reboot().await?;
    let agent = vm.wait_for_reset().await?;
    agent.ping().await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Configure Guest VSM and reboot the VM to verify it works.
#[vmm_test(
    hyperv_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64_prepped)),
)]
#[cfg_attr(not(windows), expect(dead_code))]
async fn reboot_into_guest_vsm<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
) -> Result<(), anyhow::Error> {
    let (mut vm, agent) = config.run().await?;
    let shell = agent.windows_shell();

    // VBS should be off by default
    let output = cmd!(shell, "systeminfo").output().await?;
    let output_str = String::from_utf8_lossy(&output.stdout);
    assert!(!output_str.contains("Virtualization-based security: Status: Running"));

    // Enable VBS
    cmd!(shell, "reg")
        .args([
            "add",
            "HKLM\\SYSTEM\\CurrentControlSet\\Control\\DeviceGuard",
            "/v",
            "EnableVirtualizationBasedSecurity",
            "/t",
            "REG_DWORD",
            "/d",
            "1",
            "/f",
        ])
        .run()
        .await?;
    // Enable Credential Guard
    cmd!(shell, "reg")
        .args([
            "add",
            "HKLM\\SYSTEM\\CurrentControlSet\\Control\\Lsa",
            "/v",
            "LsaCfgFlags",
            "/t",
            "REG_DWORD",
            "/d",
            "2",
            "/f",
        ])
        .run()
        .await?;
    // Enable HVCI
    cmd!(shell, "reg")
        .args([
            "add",
            "HKLM\\SYSTEM\\CurrentControlSet\\Control\\DeviceGuard\\Scenarios\\HypervisorEnforcedCodeIntegrity",
            "/v",
            "Enabled",
            "/t",
            "REG_DWORD",
            "/d",
            "1",
            "/f",
        ])
        .run()
        .await?;

    agent.reboot().await?;
    let agent = vm.wait_for_reset().await?;
    let shell = agent.windows_shell();

    // Verify VBS is running
    let output = cmd!(shell, "systeminfo").output().await?;
    let output_str = String::from_utf8_lossy(&output.stdout);
    assert!(output_str.contains("Virtualization-based security: Status: Running"));
    let output_running = &output_str[output_str.find("Services Running:").unwrap()..];
    assert!(output_running.contains("Credential Guard"));
    assert!(output_running.contains("Hypervisor enforced Code Integrity"));

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Basic boot test with secure boot enabled and a valid template.
#[vmm_test(
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_uefi_x64(vhd(ubuntu_2504_server_x64)),
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    hyperv_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    hyperv_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_uefi_x64(vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))
)]
async fn secure_boot<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let (vm, agent) = config.with_secure_boot().run().await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Verify that secure boot fails with a mismatched template.
/// TODO: Allow Hyper-V VMs to load a UEFI firmware per VM, not system wide.
#[vmm_test_with(noagent(
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_uefi_x64(vhd(ubuntu_2504_server_x64)),
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    // hyperv_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    // hyperv_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    // hyperv_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    // hyperv_uefi_x64(vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))
))]
async fn secure_boot_mismatched_template<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
) -> anyhow::Result<()> {
    let config = config
        .with_expect_boot_failure()
        .with_secure_boot()
        .with_uefi_frontpage(false);
    let config = match config.os_flavor() {
        OsFlavor::Windows => config.with_uefi_ca_secure_boot_template(),
        OsFlavor::Linux => config.with_windows_secure_boot_template(),
        _ => anyhow::bail!("Unsupported OS flavor for test: {:?}", config.os_flavor()),
    };
    let vm = config.run_without_agent().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Test EFI diagnostics with no boot devices.
/// TODO:
///   - uefi_x64 + uefi_aarch64 trace searching support
#[vmm_test_with(noagent(
    hyperv_openhcl_uefi_x64(none),
    hyperv_openhcl_uefi_aarch64(none),
    openvmm_openhcl_uefi_x64(none)
))]
async fn efi_diagnostics_no_boot<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
) -> anyhow::Result<()> {
    let vm = config.with_uefi_frontpage(true).run_without_agent().await?;

    // Expected no-boot message.
    const NO_BOOT_MSG: &str = "[Bds] Unable to boot!";

    // Get kmsg stream
    let mut kmsg = vm.kmsg().await?;

    // Search for the message
    while let Some(data) = kmsg.next().await {
        let data = data.context("reading kmsg")?;
        let msg = kmsg::KmsgParsedEntry::new(&data).unwrap();
        let raw = msg.message.as_raw();
        if raw.contains(NO_BOOT_MSG) {
            return Ok(());
        }
    }

    anyhow::bail!("Did not find expected message in kmsg");
}

/// Boot our guest-test UEFI image, which will run some tests,
/// and then purposefully triple fault itself via an expiring
/// watchdog timer.
#[vmm_test_with(noagent(
    openvmm_uefi_x64(guest_test_uefi_x64),
    openvmm_uefi_aarch64(guest_test_uefi_aarch64),
    openvmm_openhcl_uefi_x64(guest_test_uefi_x64)
))]
async fn guest_test_uefi<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let vm = config
        .with_windows_secure_boot_template()
        .run_without_agent()
        .await?;
    let arch = vm.arch();
    // No boot event check, UEFI watchdog gets fired before ExitBootServices
    let halt_reason = vm.wait_for_teardown().await?;
    tracing::debug!("vm halt reason: {halt_reason:?}");
    match arch {
        MachineArch::X86_64 => assert!(matches!(halt_reason, PetriHaltReason::TripleFault)),
        MachineArch::Aarch64 => assert!(matches!(halt_reason, PetriHaltReason::Reset)),
    }
    Ok(())
}

/// Test that unauthenticated deletion of PK and KEK is rejected by the firmware.
/// With secure boot enabled, PK and KEK are authenticated variables. An unsigned
/// delete (e.g. `rm` via efivarfs) must fail, leaving the variables intact and
/// SetupMode unchanged.
#[vmm_test(
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2404_server_x64)),
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    openvmm_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))
)]
async fn secure_boot_pk_kek_unauthenticated_delete_rejected<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
) -> anyhow::Result<()> {
    let (vm, agent) = config.with_secure_boot().run().await?;
    let shell = agent.unix_shell();

    const EFI_GLOBAL_VARIABLE_GUID: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";

    let pk_path = format!("/sys/firmware/efi/efivars/PK-{}", EFI_GLOBAL_VARIABLE_GUID);
    let kek_path = format!("/sys/firmware/efi/efivars/KEK-{}", EFI_GLOBAL_VARIABLE_GUID);
    let setup_mode_path = format!(
        "/sys/firmware/efi/efivars/SetupMode-{}",
        EFI_GLOBAL_VARIABLE_GUID
    );

    // Verify initial state: PK and KEK exist, SetupMode is 0
    let pk_exists = cmd!(shell, "sudo")
        .args(["test", "-f", &pk_path])
        .output()
        .await?;
    assert!(pk_exists.status.success(), "PK should exist initially");

    let kek_exists = cmd!(shell, "sudo")
        .args(["test", "-f", &kek_path])
        .output()
        .await?;
    assert!(kek_exists.status.success(), "KEK should exist initially");

    let setup_mode = cmd!(shell, "sudo")
        .args([
            "sh",
            "-c",
            &format!("od -An -t u1 {} | tail -c 2", setup_mode_path),
        ])
        .output()
        .await?;
    let sm = String::from_utf8_lossy(&setup_mode.stdout)
        .trim()
        .to_string();
    assert_eq!(sm, "0", "SetupMode should be 0 (secure boot active)");

    // Attempt to delete PK without authentication — should fail
    cmd!(shell, "sudo")
        .args([
            "sh",
            "-c",
            &format!("chattr -i {} 2>/dev/null || true", pk_path),
        ])
        .run()
        .await?;
    let pk_delete = cmd!(shell, "sudo")
        .args(["rm", "-f", &pk_path])
        .output()
        .await?;

    // Verify PK still exists after failed delete attempt
    let pk_still_exists = cmd!(shell, "sudo")
        .args(["test", "-f", &pk_path])
        .output()
        .await?;
    assert!(
        pk_still_exists.status.success(),
        "PK should still exist after unauthenticated delete attempt (rm exit status: {})",
        pk_delete.status,
    );

    // Attempt to delete KEK without authentication — should fail
    cmd!(shell, "sudo")
        .args([
            "sh",
            "-c",
            &format!("chattr -i {} 2>/dev/null || true", kek_path),
        ])
        .run()
        .await?;
    let kek_delete = cmd!(shell, "sudo")
        .args(["rm", "-f", &kek_path])
        .output()
        .await?;

    // Verify KEK still exists after failed delete attempt
    let kek_still_exists = cmd!(shell, "sudo")
        .args(["test", "-f", &kek_path])
        .output()
        .await?;
    assert!(
        kek_still_exists.status.success(),
        "KEK should still exist after unauthenticated delete attempt (rm exit status: {})",
        kek_delete.status,
    );

    // Verify SetupMode is still 0 — the failed deletes should not change it
    let setup_mode_after = cmd!(shell, "sudo")
        .args([
            "sh",
            "-c",
            &format!("od -An -t u1 {} | tail -c 2", setup_mode_path),
        ])
        .output()
        .await?;
    let sm_after = String::from_utf8_lossy(&setup_mode_after.stdout)
        .trim()
        .to_string();
    assert_eq!(
        sm_after, "0",
        "SetupMode should still be 0 after failed delete attempts"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Boot with a virtio-blk device served by the openvmm_vhost binary over
/// a vhost-user Unix socket.  Verifies the full stack: guest driver →
/// virtio transport → frontend protocol → socket → backend protocol →
/// virtio-blk device → disk file.
#[cfg(target_os = "linux")]
#[openvmm_test(
    linux_direct_x64[OPENVMM_VHOST_NATIVE],
    linux_direct_aarch64[OPENVMM_VHOST_NATIVE],
)]
async fn vhost_user_blk_device<T>(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    extra_deps: (petri::ResolvedArtifact<T>,),
    driver: pal_async::DefaultDriver,
) -> anyhow::Result<()> {
    use openvmm_defs::config::VirtioBus;
    use pal_async::pipe::PolledPipe;
    use pal_async::task::Spawn;
    use virtio_resources::vhost_user::VhostUserBlkHandle;
    use vm_resource::IntoResource;

    let (openvmm_vhost_artifact,) = extra_deps;
    let openvmm_vhost_path = openvmm_vhost_artifact.get();

    let log_file = config.log_source().log_file("openvmm_vhost")?;

    // Create a temporary directory for the socket and disk file.
    let tmp_dir = tempfile::tempdir().context("create temp dir")?;
    let socket_path = tmp_dir.path().join("vhost.sock");
    let disk_path = tmp_dir.path().join("test.raw");

    // Create a small raw disk file (8 MiB).
    let disk_size: u64 = 8 * 1024 * 1024;
    {
        let f = std::fs::File::create(&disk_path).context("create disk file")?;
        f.set_len(disk_size).context("set disk length")?;
    }

    // Spawn the openvmm_vhost backend process. Pipe stderr so we can
    // forward it to the petri log system.
    let (stderr_read, stderr_write) = pal::pipe_pair()?;
    let backend_child = std::process::Command::new(openvmm_vhost_path)
        .arg("--socket")
        .arg(&socket_path)
        .arg("blk")
        .arg("--disk")
        .arg(&disk_path)
        .env("RUST_LOG", "debug")
        .stdout(stderr_write.try_clone()?)
        .stderr(stderr_write)
        .spawn()
        .context("spawn openvmm_vhost")?;

    // Guard that kills the backend if the test exits early.
    struct ChildGuard(Option<std::process::Child>);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    let mut backend_guard = ChildGuard(Some(backend_child));

    // Forward backend stderr to a petri log file.
    let _log_task = driver.spawn(
        "openvmm_vhost stderr",
        petri::log_task(
            log_file,
            PolledPipe::new(&driver, stderr_read)?,
            "openvmm_vhost",
        ),
    );

    // Wait for the socket to appear (the server creates it on listen).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !socket_path.exists() {
        if std::time::Instant::now() > deadline {
            if let Some(status) = backend_guard.0.as_mut().unwrap().try_wait()? {
                anyhow::bail!("openvmm_vhost exited early with status: {status}");
            }
            anyhow::bail!(
                "timed out waiting for vhost-user socket at {}",
                socket_path.display()
            );
        }
        pal_async::timer::PolledTimer::new(&driver)
            .sleep(std::time::Duration::from_millis(50))
            .await;
    }

    // Connect to the backend and build the VM config.
    let stream =
        unix_socket::UnixStream::connect(&socket_path).context("connect to vhost-user socket")?;

    let vhost_resource = VhostUserBlkHandle {
        socket: stream.into(),
        num_queues: None,
        queue_size: None,
    }
    .into_resource();

    let (vm, agent) = config
        .modify_backend(move |b| {
            b.with_custom_config(|c| {
                c.virtio_devices.push((VirtioBus::Mmio, vhost_resource));
            })
        })
        .run()
        .await?;

    let sh = agent.unix_shell();

    // Verify the virtio-blk device appears as /dev/vda.
    let vda_size = cmd!(sh, "cat /sys/block/vda/size")
        .read()
        .await
        .context("virtio-blk device /dev/vda not found")?;
    let vda_sectors: u64 = vda_size.trim().parse().context("parse vda size")?;
    let expected_sectors = disk_size / 512;
    assert_eq!(
        vda_sectors, expected_sectors,
        "unexpected disk size in sectors"
    );

    // Write data and read it back.
    cmd!(
        sh,
        "sh -c 'echo hello_vhost_user | dd of=/dev/vda bs=512 count=1 conv=notrunc 2>/dev/null'"
    )
    .read()
    .await
    .context("write to vhost-user-blk device")?;
    let readback = cmd!(
        sh,
        "sh -c 'dd if=/dev/vda bs=512 count=1 2>/dev/null | head -c 16'"
    )
    .read()
    .await
    .context("read from vhost-user-blk device")?;
    assert!(
        readback.starts_with("hello_vhost_user"),
        "read back data mismatch: {readback}"
    );

    // Clean shutdown.
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    // The backend serves one connection and exits. Take the child out
    // of the guard so we can wait for clean exit.
    let mut backend_child = backend_guard.0.take().unwrap();
    let status = backend_child.wait().context("wait for openvmm_vhost")?;
    assert!(
        status.success(),
        "openvmm_vhost exited with non-zero status: {status}"
    );

    Ok(())
}
