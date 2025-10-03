// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use petri::PetriGuestStateLifetime;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ResolvedArtifact;
use petri::openvmm::OpenVmmPetriBackend;
use petri::run_host_cmd;
use petri_artifacts_common::tags::IsVmgsTool;
use petri_artifacts_vmm_test::artifacts::VMGSTOOL_NATIVE;
use petri_artifacts_vmm_test::artifacts::test_vmgs::VMGS_WITH_BOOT_ENTRY;
use std::process::Command;
use vmm_test_macros::openvmm_test;
use vmm_test_macros::vmm_test;
use vmm_test_macros::vmm_test_no_agent;

/// Verify that UEFI default boots even if invalid boot entries exist
/// when `default_boot_always_attempt` is enabled.
#[openvmm_test(
    openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_x64(vhd(ubuntu_2204_server_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))[VMGS_WITH_BOOT_ENTRY]
)]
async fn default_boot(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (initial_vmgs,): (ResolvedArtifact<VMGS_WITH_BOOT_ENTRY>,),
) -> Result<(), anyhow::Error> {
    let (vm, agent) = config
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .with_initial_vmgs(initial_vmgs)
        .modify_backend(|b| b.with_default_boot_always_attempt(true))
        .run()
        .await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Verify that UEFI successfully boots an operating system after reprovisioning
/// the VMGS when invalid boot entries existed initially.
#[vmm_test(
    openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_x64(vhd(ubuntu_2204_server_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))[VMGS_WITH_BOOT_ENTRY]
)]
async fn clear_vmgs<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (initial_vmgs,): (ResolvedArtifact<VMGS_WITH_BOOT_ENTRY>,),
) -> Result<(), anyhow::Error> {
    let (vm, agent) = config
        .with_guest_state_lifetime(PetriGuestStateLifetime::Reprovision)
        .with_initial_vmgs(initial_vmgs)
        .run()
        .await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Verify that UEFI fails to boot if invalid boot entries exist
///
/// This test exists to ensure we are not getting a false positive for
/// the `default_boot` and `clear_vmgs` test above.
#[vmm_test_no_agent(
    openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_x64(vhd(ubuntu_2204_server_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))[VMGS_WITH_BOOT_ENTRY]
)]
async fn invalid_boot_entries<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (initial_vmgs,): (ResolvedArtifact<VMGS_WITH_BOOT_ENTRY>,),
) -> Result<(), anyhow::Error> {
    let vm = config
        .with_expect_boot_failure()
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .with_initial_vmgs(initial_vmgs)
        .run_without_agent()
        .await?;

    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test vmgstool create command
#[vmm_test(
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))[VMGSTOOL_NATIVE],
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGSTOOL_NATIVE],
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))[VMGSTOOL_NATIVE]
)]
async fn vmgstool_create<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (vmgstool,): (ResolvedArtifact<impl IsVmgsTool>,),
) -> Result<(), anyhow::Error> {
    let temp_dir = tempfile::tempdir()?;
    let vmgs_path = temp_dir.path().join("test.vmgs");
    let vmgstool_path = vmgstool.get();

    let mut cmd = Command::new(vmgstool_path);
    cmd.arg("create").arg("--filepath").arg(&vmgs_path);
    run_host_cmd(cmd).await?;

    let (vm, agent) = config
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .with_persistent_vmgs(&vmgs_path)
        .run()
        .await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    // make sure the vmgs was actually used and that there are some boot
    // entries now. (command will fail if the fileid is not allocated)
    let mut cmd = Command::new(vmgstool_path);
    cmd.arg("uefi-nvram")
        .arg("remove-boot-entries")
        .arg("--filepath")
        .arg(&vmgs_path)
        .arg("--dry-run");
    run_host_cmd(cmd).await?;

    Ok(())
}

/// Test vmgstool remove-boot-entries command to make sure it removes the
/// invalid boot entries and the vm boots.
#[vmm_test(
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))[VMGSTOOL_NATIVE, VMGS_WITH_BOOT_ENTRY],
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGSTOOL_NATIVE, VMGS_WITH_BOOT_ENTRY],
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))[VMGSTOOL_NATIVE, VMGS_WITH_BOOT_ENTRY],
)]
async fn vmgstool_remove_boot_entries<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (vmgstool, initial_vmgs): (
        ResolvedArtifact<impl IsVmgsTool>,
        ResolvedArtifact<VMGS_WITH_BOOT_ENTRY>,
    ),
) -> Result<(), anyhow::Error> {
    let temp_dir = tempfile::tempdir()?;
    let vmgs_path = temp_dir.path().join("test.vmgs");
    let vmgstool_path = vmgstool.get();

    std::fs::copy(initial_vmgs.get(), &vmgs_path)?;

    let mut cmd = Command::new(vmgstool_path);
    cmd.arg("uefi-nvram")
        .arg("remove-boot-entries")
        .arg("--filepath")
        .arg(&vmgs_path);

    run_host_cmd(cmd).await?;

    let (vm, agent) = config
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .with_persistent_vmgs(&vmgs_path)
        .run()
        .await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}
