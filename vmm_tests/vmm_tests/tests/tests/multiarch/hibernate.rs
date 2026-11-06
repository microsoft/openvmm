// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for guest hibernate/resume and the OpenHCL hibernate token.
//!
//! Windows tests exercise an OS-managed hibernation image and resume. The
//! `guest_test_uefi` tests exercise just the platform power transition and token.

use petri::BootDeviceType;
use petri::PetriGuestStateLifetime;
use petri::PetriHaltReason;
use petri::PetriVmBuilder;
use petri::PetriVmRuntime;
use petri::PetriVmmBackend;
use petri::ProcessorTopology;
use petri::ResolvedArtifact;
use petri::openvmm::OpenVmmPetriBackend;
use petri::run_host_cmd;
use petri_artifacts_common::tags::IsVmgsTool;
use petri_artifacts_common::tags::MachineArch;
use petri_artifacts_vmm_test::artifacts::vmgstool::VMGSTOOL_NATIVE;
use std::process::Command;
use vmm_test_macros::vmm_test_with;

/// Resume the same Windows session from a hibernation image on VMBus SCSI.
#[vmm_test_with(
    openvmm,
    requires(windows_partition_reset),
    configs(
        uefi_x64(vhd(windows_datacenter_core_2025_x64)),
        uefi_aarch64(vhd(windows_11_enterprise_aarch64))
    )
)]
async fn hibernate_resume_vmbus_scsi(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    // Windows StorVSC reports the VMBus SCSI adapter as SAS.
    windows_hibernate_resume(config.with_boot_device_type(BootDeviceType::Scsi), "SAS").await
}

/// Resume the same Windows session from a hibernation image on PCIe NVMe.
#[vmm_test_with(
    openvmm,
    requires(windows_partition_reset),
    configs(
        uefi_x64(vhd(windows_datacenter_core_2025_x64)),
        uefi_aarch64(vhd(windows_11_enterprise_aarch64))
    )
)]
async fn hibernate_resume_pcie_nvme(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    windows_hibernate_resume(
        config
            .with_boot_device_type(BootDeviceType::PcieNvme)
            .with_default_boot_always_attempt(true)
            .modify_backend(|b| b.with_pcie_root_topology(1, 1, 1)),
        "NVMe",
    )
    .await
}

async fn windows_hibernate_resume(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    expected_bus: &str,
) -> anyhow::Result<()> {
    let is_aarch64 = config.arch() == MachineArch::Aarch64;
    let (mut vm, agent) = config
        .with_processor_topology(ProcessorTopology {
            // Windows ARM64 has an EFI runtime/sleep race that can deadlock
            // hibernation entry with multiple VPs. Use one VP until it is fixed.
            vp_count: if is_aarch64 { 1 } else { 2 },
            ..Default::default()
        })
        .with_hibernation_enabled(true)
        .run()
        .await?;

    let sh = agent.windows_shell();
    let boot_bus = sh
        .cmd("powershell.exe")
        .args([
            "-NoProfile",
            "-Command",
            "$ErrorActionPreference = 'Stop'; \
                     (Get-Partition -DriveLetter C | Get-Disk).BusType.ToString()",
        ])
        .read()
        .await?;
    anyhow::ensure!(
        boot_bus.trim() == expected_bus,
        "expected {expected_bus} boot disk, got {boot_bus:?}"
    );
    sh.cmd("powercfg.exe")
        .args(["/hibernate", "on"])
        .run()
        .await?;
    sh.cmd("powercfg.exe")
        .args(["/hibernate", "/type", "full"])
        .run()
        .await?;

    // The original agent process must survive, ruling out a cold boot.
    // WMI's LastBootUpTime can shift with clock adjustments across resume.
    const SESSION: &str = "$ErrorActionPreference = 'Stop'; \
        Get-Process pipette | Sort-Object Id | \
        ForEach-Object { \"$($_.Id):$($_.StartTime.ToUniversalTime().Ticks)\" }";
    let session = sh
        .cmd("powershell.exe")
        .args(["-NoProfile", "-Command", SESSION])
        .read()
        .await?;
    anyhow::ensure!(
        !session.trim().is_empty(),
        "missing Windows session identity"
    );

    // shutdown /h can remain blocked until resume; do not wait for its
    // exit over the connection that hibernation will tear down.
    let _hibernate = agent
        .command("shutdown.exe")
        .arg("/h")
        .stdin(pipette_client::process::Stdio::null())
        .spawn()
        .await?;
    // Keep Petri's watchdogs and screenshots running across the halt.
    let halt = vm.backend().wait_for_halt(false).await?;
    // Windows ARM64 can power off after writing its hibernation image.
    // The session check below must still prove this was not a shutdown.
    anyhow::ensure!(
        halt.reason == PetriHaltReason::Hibernate
            || (is_aarch64 && halt.reason == PetriHaltReason::PowerOff),
        "expected a hibernation power transition, got {halt:?}"
    );
    drop(agent);

    // Reset the hardware and boot firmware again, retaining the disk
    // overlay. Windows must restore its own hibernation image.
    vm.backend().reset().await?;
    let boot = vm.backend().wait_for_boot_event().await?;
    anyhow::ensure!(
        boot == get_resources::ged::FirmwareEvent::BootSuccess,
        "firmware failed to boot the hibernated guest: {boot:?}"
    );
    vm.backend().wait_for_enlightened_shutdown_ready().await?;
    let agent = vm.backend().wait_for_agent(false).await?;
    let resumed_session = agent
        .windows_shell()
        .cmd("powershell.exe")
        .args(["-NoProfile", "-Command", SESSION])
        .read()
        .await?;
    anyhow::ensure!(
        resumed_session == session,
        "Windows cold-booted or lost the agent process: \
                 before={session:?}, after={resumed_session:?}"
    );
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await
}

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
///
/// Only x86_64 runs in CI. The aarch64 path is fully scaffolded (guest PSCI
/// `SYSTEM_OFF2` in `guest_test_uefi`, `virt_whp` `InterceptSystemReset`, and the
/// reset-intercept mapping), but the WHP-based aarch64-windows runner does not
/// surface the hibernate intercept, so its config is omitted until the
/// hypervisor delivers it.
#[vmm_test_with(noagent, configs(openvmm_uefi_x64(guest_test_uefi_x64)))]
async fn hibernate_halts<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let vm = config
        .with_windows_secure_boot_template()
        .with_hibernation_enabled(true)
        .with_custom_uefi_json(HIBERNATE_ACTION_JSON)
        // Seeding custom UEFI NVRAM leaves the boot order empty, so force the
        // firmware to fall back to the guest_test_uefi removable-media loader
        // (required on aarch64, where the fallback is otherwise skipped).
        .with_default_boot_always_attempt(true)
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
///
/// Only x86_64 runs in CI; see `hibernate_halts` for why the aarch64 config is
/// omitted pending WHP hibernate-intercept support.
#[vmm_test_with(
    noagent,
    configs(openvmm_openhcl_uefi_x64(guest_test_uefi_x64)[VMGSTOOL_NATIVE])
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
        // The seeded CUSTOM_UEFI leaves the boot order empty, so force the
        // firmware to fall back to the guest_test_uefi removable-media loader
        // (required on aarch64, where the fallback is otherwise skipped).
        .with_default_boot_always_attempt(true)
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .with_persistent_vmgs(&vmgs_path)
        .run_without_agent()
        .await?;

    let halt_reason = vm.wait_for_halt().await?;
    if halt_reason.reason != PetriHaltReason::Hibernate {
        anyhow::bail!("expected Hibernate, got {halt_reason:?}");
    }

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
