// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for guest hibernation and the OpenHCL hibernate token.
//!
//! These boot the `guest_test_uefi` image with its `PetriBootAction=hibernate`
//! selector set, so the guest requests a real platform hibernation at startup
//! (x86_64 via the ACPI PM control register, aarch64 via PSCI `SYSTEM_OFF2`).

use petri::PetriGuestStateLifetime;
use petri::PetriHaltReason;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ResolvedArtifact;
use petri::run_host_cmd;
use petri_artifacts_common::tags::IsVmgsTool;
use petri_artifacts_vmm_test::artifacts::vmgstool::VMGSTOOL_NATIVE;
use std::process::Command;
use vmm_test_macros::vmm_test_with;

/// A `CUSTOM_UEFI` NVRAM delta that seeds the `PetriBootAction` EFI global
/// variable with the value `hibernate`, selecting the guest's hibernate action.
/// The `guid`/`attributes`/`value` fields are base64: `guid` is the EFI global
/// variable GUID (`8be4df61-93ca-11d2-aa0d-00e098032b8c`), `attributes` is
/// `0x07` (NV | BS | RT), and `value` is the ASCII string `hibernate`.
const HIBERNATE_ACTION_JSON: &[u8] = br#"{
    "type": "Microsoft.Compute/disks",
    "properties": {
        "uefiSettings": {
            "signatureMode": "Append",
            "signatures": {},
            "PetriBootAction": {
                "guid": "Yd/ki8qT0hGqDQDgmAMrjA==",
                "attributes": "Bw==",
                "value": "aGliZXJuYXRl"
            }
        }
    }
}"#;

/// Little-endian encoding of `hibernate::Token::CURRENT` (`Hibernated { 1, 9 }`,
/// i.e. `0x0109`) as written to `vmgs::FileId::HIBERNATION_TOKEN`. Keep in sync
/// with `underhill_core::hibernate::Token::CURRENT`.
const HIBERNATE_TOKEN_CURRENT: [u8; 8] = [0x09, 0x01, 0, 0, 0, 0, 0, 0];

/// Verify that a guest hibernation request halts the VM with a hibernate reason.
///
/// This exercises the non-paravisor path: the guest's power request reaches the
/// host emulation directly (x86_64 ACPI PM device, aarch64 reset intercept).
#[vmm_test_with(
    noagent,
    configs(
        openvmm_uefi_x64(guest_test_uefi_x64),
        openvmm_uefi_aarch64(guest_test_uefi_aarch64)
    )
)]
async fn hibernate_halts<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let vm = config
        .with_windows_secure_boot_template()
        .with_hibernation_enabled(true)
        .with_custom_uefi_json(HIBERNATE_ACTION_JSON)
        .run_without_agent()
        .await?;

    let halt_reason = vm.wait_for_teardown().await?;
    if halt_reason.reason != PetriHaltReason::Hibernate {
        anyhow::bail!("expected Hibernate, got {halt_reason:?}");
    }

    Ok(())
}

/// Verify that an OpenHCL guest hibernation writes the hibernate token to VMGS.
///
/// The guest requests hibernation; OpenHCL records the current firmware version
/// in `HIBERNATION_TOKEN` before notifying the host, which then surfaces a
/// hibernate halt. Afterwards the persisted token is read back from VMGS.
#[vmm_test_with(
    noagent,
    configs(
        openvmm_openhcl_uefi_x64(guest_test_uefi_x64)[VMGSTOOL_NATIVE],
        openvmm_openhcl_uefi_aarch64(guest_test_uefi_aarch64)[VMGSTOOL_NATIVE]
    )
)]
async fn hibernate_token<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (vmgstool,): (ResolvedArtifact<impl IsVmgsTool>,),
) -> anyhow::Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let vmgs_path = temp_dir.path().join("test.vmgs");
    let vmgstool_path = vmgstool.get();

    // Create a VMGS and seed the hibernate action selector into CUSTOM_UEFI.
    let mut cmd = Command::new(vmgstool_path);
    cmd.arg("create").arg("--filepath").arg(&vmgs_path);
    run_host_cmd(cmd).await?;

    let json_path = temp_dir.path().join("action.json");
    std::fs::write(&json_path, HIBERNATE_ACTION_JSON)?;

    let mut cmd = Command::new(vmgstool_path);
    cmd.arg("write")
        .arg("--filepath")
        .arg(&vmgs_path)
        .arg("--data-path")
        .arg(&json_path)
        .arg("--file-id")
        .arg("CUSTOM_UEFI");
    run_host_cmd(cmd).await?;

    // Boot: the guest reads the selector and requests hibernation.
    let mut vm = config
        .with_windows_secure_boot_template()
        .with_hibernation_enabled(true)
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .with_persistent_vmgs(&vmgs_path)
        .run_without_agent()
        .await?;

    let halt_reason = vm.wait_for_halt().await?;
    if halt_reason.reason != PetriHaltReason::Hibernate {
        anyhow::bail!("expected Hibernate, got {halt_reason:?}");
    }

    // Hyper-V VMs copy the VMGS rather than use it in-place; use the real path.
    let vmgs_path = vm.get_guest_state_file().await?.unwrap_or(vmgs_path);
    vm.teardown().await?;

    // The hibernate token must record the current firmware version.
    let token_path = temp_dir.path().join("token.bin");
    let mut cmd = Command::new(vmgstool_path);
    cmd.arg("dump")
        .arg("--filepath")
        .arg(&vmgs_path)
        .arg("--data-path")
        .arg(&token_path)
        .arg("--file-id")
        .arg("HIBERNATION_TOKEN");
    run_host_cmd(cmd).await?;

    let token = std::fs::read(&token_path)?;
    if token != HIBERNATE_TOKEN_CURRENT {
        anyhow::bail!("HIBERNATION_TOKEN = {token:02x?}, expected {HIBERNATE_TOKEN_CURRENT:02x?}");
    }

    Ok(())
}
