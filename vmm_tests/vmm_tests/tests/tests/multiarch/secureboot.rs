// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests involving UEFI Secure Boot functionality.
//! TODO: Move the remaining Secure Boot tests from other multiarch test files here.

use petri::PetriGuestStateLifetime;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ResolvedArtifact;
use petri::pipette::cmd;
use petri::run_host_cmd;
use petri_artifacts_common::tags::IsVmgsTool;
use petri_artifacts_vmm_test::artifacts::vmgstool::VMGSTOOL_NATIVE;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;
use vmm_test_macros::vmm_test;

// JSON files for custom UEFI variable deltas
const APPEND_NON_SIGNATURE_VAR_JSON: &[u8] =
    include_bytes!("custom_uefi_json/append_non_signature_var.json");
const REPLACE_DEFAULTS_WITH_NON_SIGNATURE_VAR_JSON: &[u8] =
    include_bytes!("custom_uefi_json/replace_defaults_with_non_signature_var.json");
const APPEND_SHA256_DBX_JSON: &[u8] = include_bytes!("custom_uefi_json/append_sha256_dbx.json");

// Custom UEFI variable names and values
const CUSTOM_UEFI_VAR_NAME: &str = "PetriCustomVar";
const CUSTOM_UEFI_VAR_VALUE: &[u8] = b"petri-uefi-delta";
const REPLACE_UEFI_VAR_NAME: &str = "PetriReplaceVar";
const REPLACE_UEFI_VAR_VALUE: &[u8] = b"petri-uefi-replace";
const APPENDED_DBX_SHA256: &[u8; 32] = b"petri-uefi-sha256-test-digest!!!";
const EFI_GLOBAL_VARIABLE_GUID: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";
const IMAGE_SECURITY_DATABASE_GUID: &str = "d719b2cb-3d3a-4596-a3bc-dad00e67656f";

/// Helper function to create a VMGS file with a custom UEFI variable delta JSON file.
async fn create_custom_uefi_vmgs(
    vmgstool: &Path,
    json: &[u8],
) -> Result<(TempDir, PathBuf), anyhow::Error> {
    let temp_dir = tempfile::tempdir()?;
    let vmgs_path = temp_dir.path().join("test.vmgs");
    let json_path = temp_dir.path().join("custom_uefi.json");
    std::fs::write(&json_path, json)?;

    let mut create = Command::new(vmgstool);
    create.arg("create").arg("--filepath").arg(&vmgs_path);
    run_host_cmd(create).await?;

    let mut write = Command::new(vmgstool);
    write
        .arg("write")
        .arg("--filepath")
        .arg(&vmgs_path)
        .arg("--data-path")
        .arg(&json_path)
        .arg("--file-id")
        .arg("CUSTOM_UEFI");
    run_host_cmd(write).await?;

    Ok((temp_dir, vmgs_path))
}

/// Verify that custom UEFI variable deltas are applied on first boot.
///
/// Direct UEFI receives the JSON through Petri configuration. OpenHCL receives
/// the same JSON through the production VMGS `CUSTOM_UEFI` path because it owns
/// the UEFI device inside VTL2.
#[vmm_test(
    // TODO: Re-enable once direct OpenVMM x64 CI jobs provide vmgstool.
    // openvmm_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGSTOOL_NATIVE],
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGSTOOL_NATIVE],
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGSTOOL_NATIVE],
    openvmm_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGSTOOL_NATIVE],
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGSTOOL_NATIVE],
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGSTOOL_NATIVE]
)]
async fn custom_uefi_append_non_signature_var<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (vmgstool,): (ResolvedArtifact<impl IsVmgsTool>,),
) -> Result<(), anyhow::Error> {
    let (_temp_dir, vmgs_path) =
        create_custom_uefi_vmgs(vmgstool.get(), APPEND_NON_SIGNATURE_VAR_JSON).await?;

    let (vm, agent) = config
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .with_persistent_vmgs(&vmgs_path)
        .with_uefi_ca_secure_boot_template()
        .with_custom_uefi_json(APPEND_NON_SIGNATURE_VAR_JSON)
        .run()
        .await?;

    let shell = agent.unix_shell();
    let var_path =
        format!("/sys/firmware/efi/efivars/{CUSTOM_UEFI_VAR_NAME}-{EFI_GLOBAL_VARIABLE_GUID}");
    let output = cmd!(shell, "sudo")
        .args([
            "dd",
            &format!("if={var_path}"),
            "bs=4",
            "skip=1",
            "status=none",
        ])
        .output()
        .await?;
    assert!(output.status.success(), "custom UEFI variable should exist");
    assert_eq!(output.stdout, CUSTOM_UEFI_VAR_VALUE);

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Verify that replacing Secure Boot signatures with their base-template
/// defaults also applies non-signature variables.
#[vmm_test(
    // TODO: Re-enable once direct OpenVMM x64 CI jobs provide vmgstool.
    // openvmm_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGSTOOL_NATIVE],
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGSTOOL_NATIVE],
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGSTOOL_NATIVE],
    openvmm_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGSTOOL_NATIVE],
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGSTOOL_NATIVE],
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGSTOOL_NATIVE]
)]
async fn custom_uefi_replace_defaults<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (vmgstool,): (ResolvedArtifact<impl IsVmgsTool>,),
) -> Result<(), anyhow::Error> {
    let (_temp_dir, vmgs_path) =
        create_custom_uefi_vmgs(vmgstool.get(), REPLACE_DEFAULTS_WITH_NON_SIGNATURE_VAR_JSON)
            .await?;

    let (vm, agent) = config
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .with_persistent_vmgs(&vmgs_path)
        .with_uefi_ca_secure_boot_template()
        .with_custom_uefi_json(REPLACE_DEFAULTS_WITH_NON_SIGNATURE_VAR_JSON)
        .run()
        .await?;

    let shell = agent.unix_shell();
    let var_path =
        format!("/sys/firmware/efi/efivars/{REPLACE_UEFI_VAR_NAME}-{EFI_GLOBAL_VARIABLE_GUID}");
    let output = cmd!(shell, "sudo")
        .args([
            "dd",
            &format!("if={var_path}"),
            "bs=4",
            "skip=1",
            "status=none",
        ])
        .output()
        .await?;
    assert!(
        output.status.success(),
        "replacement UEFI variable should exist"
    );
    assert_eq!(output.stdout, REPLACE_UEFI_VAR_VALUE);

    let pk_path = format!("/sys/firmware/efi/efivars/PK-{EFI_GLOBAL_VARIABLE_GUID}");
    let pk_exists = cmd!(shell, "sudo")
        .args(["test", "-f", &pk_path])
        .output()
        .await?;
    assert!(pk_exists.status.success(), "default PK should be retained");

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Verify that a SHA-256 signature delta is appended to dbx.
#[vmm_test(
    // TODO: Re-enable once direct OpenVMM x64 CI jobs provide vmgstool.
    // openvmm_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGSTOOL_NATIVE],
    openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGSTOOL_NATIVE],
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGSTOOL_NATIVE],
    openvmm_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGSTOOL_NATIVE],
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGSTOOL_NATIVE],
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGSTOOL_NATIVE]
)]
async fn custom_uefi_append_sha256_dbx<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (vmgstool,): (ResolvedArtifact<impl IsVmgsTool>,),
) -> Result<(), anyhow::Error> {
    let (_temp_dir, vmgs_path) =
        create_custom_uefi_vmgs(vmgstool.get(), APPEND_SHA256_DBX_JSON).await?;

    let (vm, agent) = config
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .with_persistent_vmgs(&vmgs_path)
        .with_uefi_ca_secure_boot_template()
        .with_custom_uefi_json(APPEND_SHA256_DBX_JSON)
        .run()
        .await?;

    let shell = agent.unix_shell();
    let dbx_path = format!("/sys/firmware/efi/efivars/dbx-{IMAGE_SECURITY_DATABASE_GUID}");
    let output = cmd!(shell, "sudo")
        .args([
            "dd",
            &format!("if={dbx_path}"),
            "bs=4",
            "skip=1",
            "status=none",
        ])
        .output()
        .await?;
    assert!(output.status.success(), "dbx should exist");
    assert!(
        output
            .stdout
            .windows(APPENDED_DBX_SHA256.len())
            .any(|bytes| bytes == APPENDED_DBX_SHA256),
        "appended SHA-256 digest should be present in dbx"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
