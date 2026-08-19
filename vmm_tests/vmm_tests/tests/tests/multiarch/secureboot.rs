// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests involving UEFI Secure Boot functionality.
//! TODO: Move the remaining Secure Boot tests from other multiarch test files here.

use anyhow::Context;
use futures::StreamExt;
use mesh::CancelContext;
use petri::PetriGuestStateLifetime;
use petri::PetriVm;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ResolvedArtifact;
use petri::pipette::cmd;
use petri::run_host_cmd;
use petri_artifacts_common::tags::IsVmgsTool;
use petri_artifacts_vmm_test::artifacts::vmgstool::VMGSTOOL_NATIVE;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
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

// Test helpers

/// Create a VMGS file, optionally provisioned with a custom UEFI variable delta.
async fn create_uefi_vmgs(
    vmgstool: &Path,
    custom_uefi_json: Option<&[u8]>,
) -> Result<(TempDir, PathBuf), anyhow::Error> {
    let temp_dir = tempfile::tempdir()?;
    let vmgs_path = temp_dir.path().join("test.vmgs");

    let mut create = Command::new(vmgstool);
    create.arg("create").arg("--filepath").arg(&vmgs_path);
    run_host_cmd(create).await?;

    if let Some(json) = custom_uefi_json {
        let json_path = temp_dir.path().join("custom_uefi.json");
        std::fs::write(&json_path, json)?;

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
    }

    Ok((temp_dir, vmgs_path))
}

/// Wait for and validate the Secure Boot variable reports.
async fn verify_secure_boot_variable_reports<T: PetriVmmBackend>(
    vm: &PetriVm<T>,
    has_custom_uefi: bool,
) -> Result<(), anyhow::Error> {
    const EXPECTED_VARIABLES: [&str; 4] = ["PK", "KEK", "db", "dbx"];

    let mut kmsg = vm.kmsg().await?;
    let mut reported_variables = BTreeSet::new();

    while let Some(data) = kmsg.next().await {
        let data = data.context("reading kmsg")?;
        let message = kmsg::KmsgParsedEntry::new(&data).unwrap();
        let raw = message.message.as_raw();

        if !raw.contains("SecureBootVar") {
            continue;
        }
        let Some(variable_name) = EXPECTED_VARIABLES
            .into_iter()
            .find(|name| raw.contains(&format!("name: {name:?}")))
        else {
            continue;
        };

        assert!(
            raw.contains("in_nvram: true"),
            "Secure Boot variable is unexpectedly absent from NVRAM: {raw}"
        );
        assert!(
            raw.contains(&format!("has_custom_uefi: {has_custom_uefi}")),
            "unexpected custom UEFI presence in variable report: {raw}"
        );
        assert!(
            raw.contains("missing_entries: 0x0"),
            "baseline entries missing from report: {raw}"
        );
        reported_variables.insert(variable_name);

        if reported_variables.len() == EXPECTED_VARIABLES.len() {
            return Ok(());
        }
    }

    let expected_variables = BTreeSet::from(EXPECTED_VARIABLES);
    let missing_variables: Vec<_> = expected_variables.difference(&reported_variables).collect();
    anyhow::bail!("missing Secure Boot variable reports: {missing_variables:?}")
}

/// Boot with optional custom UEFI configuration and verify its Secure Boot variable reports.
async fn secure_boot_variable_report_test<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    vmgstool: &Path,
    custom_uefi_json: Option<&[u8]>,
) -> Result<(), anyhow::Error> {
    let (_temp_dir, vmgs_path) = create_uefi_vmgs(vmgstool, custom_uefi_json).await?;
    let mut config = config
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .with_persistent_vmgs(&vmgs_path)
        .with_secure_boot();
    if let Some(json) = custom_uefi_json {
        config = config.with_custom_uefi_json(json);
    }

    let (vm, agent) = config.run().await?;
    CancelContext::new()
        .with_timeout(Duration::from_secs(60))
        .until_cancelled(verify_secure_boot_variable_reports(
            &vm,
            custom_uefi_json.is_some(),
        ))
        .await
        .context("Secure Boot reports were not observed within 60 seconds")??;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

// Test cases

#[vmm_test(
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGSTOOL_NATIVE],
    openvmm_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGSTOOL_NATIVE],
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGSTOOL_NATIVE],
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGSTOOL_NATIVE]
)]
async fn secure_boot_variable_reports_without_custom_uefi<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (vmgstool,): (ResolvedArtifact<impl IsVmgsTool>,),
) -> Result<(), anyhow::Error> {
    secure_boot_variable_report_test(config, vmgstool.get(), None).await
}

#[vmm_test(
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGSTOOL_NATIVE],
    openvmm_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGSTOOL_NATIVE],
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGSTOOL_NATIVE],
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[VMGSTOOL_NATIVE]
)]
async fn secure_boot_variable_reports_with_custom_uefi<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (vmgstool,): (ResolvedArtifact<impl IsVmgsTool>,),
) -> Result<(), anyhow::Error> {
    secure_boot_variable_report_test(config, vmgstool.get(), Some(APPEND_NON_SIGNATURE_VAR_JSON))
        .await
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
        create_uefi_vmgs(vmgstool.get(), Some(APPEND_NON_SIGNATURE_VAR_JSON)).await?;

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
    let (_temp_dir, vmgs_path) = create_uefi_vmgs(
        vmgstool.get(),
        Some(REPLACE_DEFAULTS_WITH_NON_SIGNATURE_VAR_JSON),
    )
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
        create_uefi_vmgs(vmgstool.get(), Some(APPEND_SHA256_DBX_JSON)).await?;

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
