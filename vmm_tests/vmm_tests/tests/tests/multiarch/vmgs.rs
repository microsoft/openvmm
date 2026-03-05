// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use petri::CommandError;
use petri::PetriGuestStateLifetime;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ResolvedArtifact;
use petri::run_host_cmd;
use petri_artifacts_common::tags::IsVmgsTool;
use petri_artifacts_vmm_test::artifacts::VMGSTOOL_NATIVE;
use petri_artifacts_vmm_test::artifacts::test_vmgs::VMGS_WITH_BOOT_ENTRY;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;
use vmgs_resources::GuestStateEncryptionPolicy;
use vmm_test_macros::openvmm_test;
use vmm_test_macros::vmm_test;
use vmm_test_macros::vmm_test_no_agent;

/// Verify that UEFI default boots even if invalid boot entries exist
/// when `default_boot_always_attempt` is enabled.
#[vmm_test(
    openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGS_WITH_BOOT_ENTRY],
    hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64))[VMGS_WITH_BOOT_ENTRY],
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGS_WITH_BOOT_ENTRY],
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGS_WITH_BOOT_ENTRY]
)]
async fn default_boot<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (initial_vmgs,): (ResolvedArtifact<VMGS_WITH_BOOT_ENTRY>,),
) -> Result<(), anyhow::Error> {
    let (vm, agent) = config
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .with_initial_vmgs(initial_vmgs)
        .with_default_boot_always_attempt(true)
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
    openvmm_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGS_WITH_BOOT_ENTRY],
    hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64))[VMGS_WITH_BOOT_ENTRY],
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGS_WITH_BOOT_ENTRY],
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGS_WITH_BOOT_ENTRY]
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
    openvmm_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGS_WITH_BOOT_ENTRY],
    hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64))[VMGS_WITH_BOOT_ENTRY],
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGS_WITH_BOOT_ENTRY],
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))[VMGS_WITH_BOOT_ENTRY],
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGS_WITH_BOOT_ENTRY]
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
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGSTOOL_NATIVE],
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGSTOOL_NATIVE],
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGSTOOL_NATIVE]
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

    let (mut vm, agent) = config
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .with_persistent_vmgs(&vmgs_path)
        .run()
        .await?;

    agent.power_off().await?;
    vm.wait_for_clean_shutdown().await?;

    // Hyper-V VMs copy the VMGS rather than use it in-place, so update the
    // path here.
    let vmgs_path = vm.get_guest_state_file().await?.unwrap_or(vmgs_path);

    run_vmgstool_verification(vmgstool_path, &vmgs_path, None, &temp_dir).await?;

    vm.teardown().await?;

    Ok(())
}

/// Test vmgstool remove-boot-entries command to make sure it removes the
/// invalid boot entries and the vm boots.
#[vmm_test(
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGSTOOL_NATIVE, VMGS_WITH_BOOT_ENTRY],
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGSTOOL_NATIVE, VMGS_WITH_BOOT_ENTRY],
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGSTOOL_NATIVE, VMGS_WITH_BOOT_ENTRY],
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

/// Test key for encrypting VMGS that matches test seed and bios guid in the
/// openvmm guest emulation device.
const TEST_GSP_BY_ID: [u8; 32] = [
    0x30, 0x4, 0xE2, 0x1E, 0x2E, 0x5E, 0x26, 0xDA, 0x18, 0xFA, 0x7F, 0x3, 0x8, 0x29, 0xB8, 0x91,
    0x61, 0xAD, 0x54, 0xB4, 0xAC, 0x4D, 0x9A, 0xEF, 0x72, 0xB3, 0x28, 0x41, 0xF2, 0xB7, 0x5, 0x1A,
];

/// Test vmgstool encryption
#[openvmm_test(
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGSTOOL_NATIVE],
)]
async fn vmgstool_encryption<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (vmgstool,): (ResolvedArtifact<impl IsVmgsTool>,),
) -> Result<(), anyhow::Error> {
    let temp_dir = tempfile::tempdir()?;
    let vmgs_path = temp_dir.path().join("test.vmgs");
    let key_path = temp_dir.path().join("key.bin");
    let vmgstool_path = vmgstool.get();

    std::fs::write(&key_path, TEST_GSP_BY_ID)?;

    let mut cmd = Command::new(vmgstool_path);
    cmd.arg("create")
        .arg("--filepath")
        .arg(&vmgs_path)
        .arg("--keypath")
        .arg(&key_path)
        .arg("--encryptionalgorithm")
        .arg("AES_GCM");
    run_host_cmd(cmd).await?;

    let (vm, agent) = config
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .with_persistent_vmgs(&vmgs_path)
        .with_guest_state_encryption(GuestStateEncryptionPolicy::GspById(true))
        .run()
        .await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    run_vmgstool_verification(vmgstool_path, &vmgs_path, Some(&key_path), &temp_dir).await?;

    Ok(())
}

/// test some vmgstool commands by verifying that the vmgs file is in a good state
async fn run_vmgstool_verification(
    vmgstool_path: &Path,
    vmgs_path: &Path,
    key_path: Option<&Path>,
    temp_dir: &TempDir,
) -> anyhow::Result<()> {
    // check that the headers are valid
    let mut cmd = Command::new(vmgstool_path);
    cmd.arg("dump-headers").arg("--filepath").arg(vmgs_path);
    run_host_cmd(cmd).await?;

    // make sure the vmgs was actually used and that there are some boot
    // entries now.
    let mut cmd = Command::new(vmgstool_path);
    cmd.arg("query-size")
        .arg("--filepath")
        .arg(vmgs_path)
        .arg("--file-id")
        .arg("1");
    run_host_cmd(cmd).await?;

    let mut cmd = Command::new(vmgstool_path);
    cmd.arg("uefi-nvram")
        .arg("remove-boot-entries")
        .arg("--filepath")
        .arg(vmgs_path)
        .arg("--dry-run");
    if let Some(key_path) = key_path {
        cmd.arg("--keypath").arg(key_path);
    }
    run_host_cmd(cmd).await?;

    // check encryption
    let mut cmd = Command::new(vmgstool_path);
    cmd.arg("query-encryption").arg("--filepath").arg(vmgs_path);
    let res = run_host_cmd(cmd).await;
    let code = match res {
        Ok(_) => Some(0),
        Err(CommandError::Command(status, _)) => status.code(),
        _ => None,
    };
    if key_path.is_none() {
        // No encryption
        assert_eq!(code, Some(2));
    } else {
        // GspById
        assert_eq!(code, Some(6));
    }

    // write and read
    let data_path = temp_dir.path().join("test.bin");
    let contents = b"hello world";
    std::fs::write(&data_path, contents)?;

    let mut cmd = Command::new(vmgstool_path);
    cmd.arg("write")
        .arg("--filepath")
        .arg(vmgs_path)
        .arg("--data-path")
        .arg(&data_path)
        .arg("--fileid")
        .arg("3")
        .arg("--allow-overwrite");
    if let Some(key_path) = key_path {
        cmd.arg("--keypath").arg(key_path);
    }
    run_host_cmd(cmd).await?;

    let out_data_path = temp_dir.path().join("out.bin");
    let mut cmd = Command::new(vmgstool_path);
    cmd.arg("dump")
        .arg("--filepath")
        .arg(vmgs_path)
        .arg("--data-path")
        .arg(&out_data_path)
        .arg("--fileid")
        .arg("3");
    if let Some(key_path) = key_path {
        cmd.arg("--keypath").arg(key_path);
    }
    run_host_cmd(cmd).await?;

    let output = std::fs::read(&out_data_path)?;
    assert_eq!(&contents[..], &output[..]);

    let mut cmd = Command::new(vmgstool_path);
    cmd.arg("move")
        .arg("--filepath")
        .arg(vmgs_path)
        .arg("--src")
        .arg("3")
        .arg("--dst")
        .arg("17");
    if let Some(key_path) = key_path {
        cmd.arg("--keypath").arg(key_path);
    }
    run_host_cmd(cmd).await?;

    let out_data_path = temp_dir.path().join("out.bin");
    let mut cmd = Command::new(vmgstool_path);
    cmd.arg("dump")
        .arg("--filepath")
        .arg(vmgs_path)
        .arg("--data-path")
        .arg(&out_data_path)
        .arg("--fileid")
        .arg("17");
    if let Some(key_path) = key_path {
        cmd.arg("--keypath").arg(key_path);
    }
    run_host_cmd(cmd).await?;

    let output = std::fs::read(&out_data_path)?;
    assert_eq!(&contents[..], &output[..]);

    Ok(())
}

/// Test vmgstool encryption
#[openvmm_test(
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGSTOOL_NATIVE],
)]
async fn vmgstool_update_key<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (vmgstool,): (ResolvedArtifact<impl IsVmgsTool>,),
) -> Result<(), anyhow::Error> {
    let temp_dir = tempfile::tempdir()?;
    let vmgs_path = temp_dir.path().join("test.vmgs");
    let key1_path = temp_dir.path().join("key1.bin");
    let key2_path = temp_dir.path().join("key2.bin");
    let key3_path = temp_dir.path().join("key3.bin");
    let vmgstool_path = vmgstool.get();

    std::fs::write(&key3_path, TEST_GSP_BY_ID)?;

    let mut cmd = Command::new(vmgstool_path);
    cmd.arg("test")
        .arg("two-keys")
        .arg("--filepath")
        .arg(&vmgs_path)
        .arg("--first-key-path")
        .arg(&key1_path)
        .arg("--second-key-path")
        .arg(&key2_path);
    run_host_cmd(cmd).await?;

    let mut cmd = Command::new(vmgstool_path);
    cmd.arg("update-key")
        .arg("--filepath")
        .arg(&vmgs_path)
        .arg("--keypath")
        .arg(&key2_path)
        .arg("--newkeypath")
        .arg(&key3_path)
        .arg("--encryptionalgorithm")
        .arg("AES_GCM");
    run_host_cmd(cmd).await?;

    let (vm, agent) = config
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .with_persistent_vmgs(&vmgs_path)
        .with_guest_state_encryption(GuestStateEncryptionPolicy::GspById(true))
        .run()
        .await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}
