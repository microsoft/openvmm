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
use vmm_test_macros::openvmm_test_no_agent;
use vmm_test_macros::vmm_test;
use vmm_test_macros::vmm_test_no_agent;

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
/// Tests of vmbus relay functionality.
mod vmbus_relay;
/// Tests involving VMGS functionality
mod vmgs;

/// Boot through the UEFI firmware, it will shut itself down after booting.
#[vmm_test_no_agent(
    openvmm_uefi_x64(none),
    openvmm_openhcl_uefi_x64(none),
    openvmm_uefi_aarch64(none),
    hyperv_openhcl_uefi_aarch64(none),
    hyperv_openhcl_uefi_x64(none)
)]
async fn frontpage<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let vm = config.run_without_agent().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Basic boot test.
#[vmm_test(
    openvmm_linux_direct_x64,
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
    hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2404_server_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    openvmm_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped)),
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

/// Basic boot test without agent
#[vmm_test_no_agent(
    openvmm_pcat_x64(vhd(freebsd_13_2_x64)),
    openvmm_pcat_x64(iso(freebsd_13_2_x64))
)]
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
    hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    openvmm_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped)),
    // openvmm_openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[tdx](vhd(ubuntu_2504_server_x64))
)]
async fn boot_heavy<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let is_openhcl = config.is_openhcl();
    let (vm, agent) = config
        .with_processor_topology(ProcessorTopology {
            vp_count: 16,
            vps_per_socket: Some(8),
            ..Default::default()
        })
        // multiarch::openvmm_uefi_x64_windows_datacenter_core_2022_x64_boot_heavy
        // fails with 4GB of RAM (the default), and openhcl tests fail with 1GB.
        .with_memory(MemoryConfig {
            startup_bytes: if is_openhcl { 4 * SIZE_1_GB } else { SIZE_1_GB },
            ..Default::default()
        })
        .run()
        .await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Basic boot test with a single VP.
#[vmm_test(
    openvmm_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped)),
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

#[cfg(windows)] // requires VPCI support, which is only on Windows right now
#[vmm_test(
    // TODO: virt_whp is missing VPCI LPI interrupt support, used by Windows (but not Linux)
    // openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    // TODO: Linux image is missing VPCI driver in its initrd
    // openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    // openvmm_uefi_x64(vhd(ubuntu_2504_server_x64))
)]
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
#[cfg(windows)] // requires VPCI support, which is only on Windows right now
#[vmm_test(
    // TODO: aarch64 support (WHP missing ARM64 VTL2 support)
    // openvmm_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    // openvmm_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    // TODO: Linux image is missing VPCI driver in its initrd
    // openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))
)]
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
    // openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))
    hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    openvmm_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped)),
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
#[vmm_test_no_agent(
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
)]
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

/// Test EFI diagnostics with no boot devices on OpenVMM.
/// TODO:
///   - kmsg support in Hyper-V
///   - openhcl_uefi_aarch64 support
///   - uefi_x64 + uefi_aarch64 trace searching support
#[openvmm_test_no_agent(openhcl_uefi_x64(none))]
async fn efi_diagnostics_no_boot(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
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
#[vmm_test_no_agent(
    openvmm_uefi_x64(guest_test_uefi_x64),
    openvmm_uefi_aarch64(guest_test_uefi_aarch64),
    openvmm_openhcl_uefi_x64(guest_test_uefi_x64)
)]
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

/// Test that PK and KEK variables can be updated/deleted by verifying
/// that they are writeable at runtime.
#[vmm_test(
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2404_server_x64)),
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    openvmm_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))
)]
async fn secure_boot_pk_kek_writable<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
) -> anyhow::Result<()> {
    let (vm, agent) = config.with_secure_boot().run().await?;
    let shell = agent.unix_shell();

    // EFI Global Variable GUID
    const EFI_GLOBAL_VARIABLE_GUID: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";

    // Check if the EFI variables exist and inspect their permissions
    let pk_path = format!("/sys/firmware/efi/efivars/PK-{}", EFI_GLOBAL_VARIABLE_GUID);
    let kek_path = format!("/sys/firmware/efi/efivars/KEK-{}", EFI_GLOBAL_VARIABLE_GUID);

    let pk_attrs = cmd!(shell, "sudo")
        .args([
            "sh",
            "-c",
            &format!("ls -l {} 2>&1 && stat -c '%a' {} 2>&1", pk_path, pk_path),
        ])
        .output()
        .await?;

    let kek_attrs = cmd!(shell, "sudo")
        .args([
            "sh",
            "-c",
            &format!("ls -l {} 2>&1 && stat -c '%a' {} 2>&1", kek_path, kek_path),
        ])
        .output()
        .await?;

    let pk_info = String::from_utf8_lossy(&pk_attrs.stdout);
    let kek_info = String::from_utf8_lossy(&kek_attrs.stdout);

    // The file permissions in octal should be 644
    // (read/write for owner, read for group/others)
    assert!(
        pk_info.contains("644") || pk_info.contains("rw-"),
        "PK should be writable at runtime. File info: {}",
        pk_info
    );
    assert!(
        kek_info.contains("644") || kek_info.contains("rw-"),
        "KEK should be writable at runtime. File info: {}",
        kek_info
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Test that deleting PK puts the system into Setup Mode.
/// When PK is deleted, the system enters Setup Mode (SetupMode=1), allowing reconfiguration
/// of secure boot variables without authentication.
#[vmm_test(
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2404_server_x64)),
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    openvmm_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))
)]
async fn secure_boot_pk_delete_enters_setup_mode<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
) -> anyhow::Result<()> {
    let (vm, agent) = config.with_secure_boot().run().await?;
    let shell = agent.unix_shell();

    // EFI Global Variable GUID
    const EFI_GLOBAL_VARIABLE_GUID: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";

    let pk_path = format!("/sys/firmware/efi/efivars/PK-{}", EFI_GLOBAL_VARIABLE_GUID);
    let setup_mode_path = format!(
        "/sys/firmware/efi/efivars/SetupMode-{}",
        EFI_GLOBAL_VARIABLE_GUID
    );

    // Check initial state: SetupMode should be disabled (0)
    let setup_mode_before = cmd!(shell, "sudo")
        .args([
            "sh",
            "-c",
            &format!("od -An -t u1 {} | tail -c 2", setup_mode_path),
        ])
        .output()
        .await?;

    let sm_before = String::from_utf8_lossy(&setup_mode_before.stdout)
        .trim()
        .to_string();

    assert_eq!(
        sm_before, "0",
        "SetupMode should be disabled (0) before deleting PK, got: {}",
        sm_before
    );

    // Remove immutable flag if set, then delete PK
    cmd!(shell, "sudo")
        .args([
            "sh",
            "-c",
            &format!("chattr -i {} 2>/dev/null || true", pk_path),
        ])
        .run()
        .await?;

    let delete_result = cmd!(shell, "sudo")
        .args(["rm", "-f", &pk_path])
        .output()
        .await?;

    assert!(
        delete_result.status.success(),
        "Failed to delete PK: {}",
        String::from_utf8_lossy(&delete_result.stderr)
    );

    // Verify PK is deleted - use ls which returns success even if file doesn't exist with || true
    let pk_check = cmd!(shell, "sh")
        .args([
            "-c",
            &format!("ls {} 2>/dev/null || echo 'file_not_found'", pk_path),
        ])
        .output()
        .await?;
    let pk_check_out = String::from_utf8_lossy(&pk_check.stdout);
    assert!(
        pk_check_out.contains("file_not_found") || pk_check_out.contains("No such file"),
        "PK should be deleted but still exists: {}",
        pk_check_out
    );

    // Check state after deletion: SetupMode should be enabled (1)
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
        sm_after, "1",
        "SetupMode should be enabled (1) after deleting PK, got: {}",
        sm_after
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
