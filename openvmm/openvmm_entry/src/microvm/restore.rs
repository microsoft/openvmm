// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM snapshot restore-time contract checks.

use super::filesystem::microvm_filesystem_slot_from_snapshot;
use crate::Options;
use crate::cli_args;
use crate::cli_args::DiskCliKind;
use crate::cli_args::microvm::MachineProfileCli;
use anyhow::Context;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::microvm::MicrovmFilesystemConfig;
use openvmm_defs::microvm::MicrovmNetworkConfig;
use openvmm_helpers::snapshot::SnapshotManifest;
use openvmm_helpers::snapshot::microvm::SnapshotAttachment;
use openvmm_helpers::snapshot::microvm::SnapshotMachineContract;
use openvmm_helpers::snapshot::microvm::SnapshotMicrovmSandboxBlock;
use openvmm_helpers::snapshot::restore::OpenedSnapshot;
use std::path::Path;
use std::time::Duration;

const MAX_SNAPSHOT_DOWNTIME: Duration = Duration::from_secs(30 * 24 * 60 * 60);

fn calculate_snapshot_downtime(
    capture_time: std::time::SystemTime,
    destination_time: std::time::SystemTime,
) -> anyhow::Result<Duration> {
    let downtime = destination_time
        .duration_since(capture_time)
        .context("destination wall clock is before snapshot capture time")?;
    anyhow::ensure!(
        downtime <= MAX_SNAPSHOT_DOWNTIME,
        "snapshot host downtime exceeds the supported 30-day bound"
    );
    Ok(downtime)
}

/// Restore-time inputs that shape a microVM configuration.
#[derive(Default)]
pub(crate) struct MicrovmRestore {
    /// The authoritative machine contract of the snapshot being restored.
    pub(crate) machine_contract: Option<SnapshotMachineContract>,
    /// Private copy of a paired scratch image, kept alive for the VM lifetime.
    pub(crate) private_scratch_dir: Option<tempfile::TempDir>,
}

/// Validates a snapshot restore against the microVM profile and prepares the
/// restore-time inputs of the microVM configuration.
///
/// This may adjust `opt`: the snapshot selects the RAM size, and a paired
/// scratch image is replaced by a private copy.
pub(crate) fn prepare_restore(
    opt: &mut Options,
    restore_snapshot: Option<&OpenedSnapshot>,
) -> anyhow::Result<MicrovmRestore> {
    let mut private_scratch_dir = None;
    if let Some(contract) =
        restore_snapshot.and_then(|snapshot| snapshot.manifest().machine_contract.as_ref())
        && contract.machine_profile == "microvm"
    {
        anyhow::ensure!(
            opt.machine == MachineProfileCli::Microvm,
            "microVM snapshot restore requires --machine microvm"
        );
        openvmm_helpers::snapshot::microvm::validate_supported_microvm_contract(contract)?;
    }
    let restore_machine_contract = if opt.machine == MachineProfileCli::Microvm
        && let Some(snapshot) = restore_snapshot
    {
        let manifest = snapshot.manifest();
        let snapshot_dir = opt
            .restore_snapshot
            .as_deref()
            .expect("restore manifest requires a snapshot path");
        let contract = manifest
            .machine_contract
            .as_ref()
            .context("microVM snapshot is missing its authoritative machine contract")?;
        anyhow::ensure!(
            contract.machine_profile == "microvm",
            "snapshot machine profile does not match the requested microVM machine"
        );
        openvmm_helpers::snapshot::microvm::validate_supported_microvm_contract(contract)?;
        anyhow::ensure!(
            opt.cmdline.is_empty(),
            "restore-time command-line overrides are not allowed"
        );
        anyhow::ensure!(
            opt.memory == Default::default()
                && !opt.deprecated_private_memory
                && !opt.deprecated_prefetch
                && !opt.deprecated_thp
                && opt.deprecated_memory_backing_file.is_none(),
            "restore-time memory overrides are not allowed"
        );
        opt.memory.size = Some(vmm_cli::MemorySize(manifest.memory_size_bytes));
        if !contract.microvm_sandbox_blocks.is_empty() {
            let scratch = contract
                .microvm_sandbox_blocks
                .last()
                .filter(|block| block.role == "scratch")
                .context("microVM snapshot is missing its scratch contract")?;
            if scratch.artifact == openvmm_helpers::snapshot::format::SCRATCH_FILE_NAME {
                anyhow::ensure!(
                    !opt.microvm.microvm_sandbox_block.iter().any(|block| {
                        block.role == openvmm_defs::microvm::MicrovmSandboxBlockRole::Scratch
                    }),
                    "paired snapshot restore supplies scratch.img; do not pass a scratch block"
                );
                let source = snapshot
                    .open_paired_scratch_file()?
                    .context("paired snapshot is missing scratch.img")?;
                let parent = snapshot_dir
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                let temp_dir = tempfile::Builder::new()
                    .prefix(".openvmm-private-scratch-")
                    .tempdir_in(parent)
                    .context("failed to create private restore scratch directory")?;
                let private_path = temp_dir
                    .path()
                    .join(openvmm_helpers::snapshot::format::SCRATCH_FILE_NAME);
                openvmm_helpers::snapshot::restore::copy_verified_file(
                    &source,
                    &private_path,
                    scratch.length,
                    &scratch.identity,
                    openvmm_helpers::snapshot::format::SCRATCH_FILE_NAME,
                )?;
                opt.microvm
                    .microvm_sandbox_block
                    .push(cli_args::microvm::MicrovmSandboxBlockCli {
                        role: openvmm_defs::microvm::MicrovmSandboxBlockRole::Scratch,
                        disk: cli_args::DiskCli {
                            vtl: DeviceVtl::Vtl0,
                            kind: DiskCliKind::File {
                                path: private_path,
                                create_with_len: None,
                                direct: false,
                            },
                            read_only: false,
                            is_dvd: false,
                            underhill: None,
                            pcie_port: None,
                            controller: None,
                            nsid: None,
                            lun: None,
                            relay: None,
                        },
                    });
                private_scratch_dir = Some(temp_dir);
            } else {
                anyhow::ensure!(
                    opt.microvm.microvm_sandbox_block.iter().any(|block| {
                        block.role == openvmm_defs::microvm::MicrovmSandboxBlockRole::Scratch
                    }),
                    "fresh-scratch snapshot restore requires a scratch block"
                );
            }
        }
        Some(contract.clone())
    } else {
        None
    };
    Ok(MicrovmRestore {
        machine_contract: restore_machine_contract,
        private_scratch_dir,
    })
}

/// The machine contract a microVM snapshot must match to be restored: the
/// hypervisor, effective command line, network, filesystem, boot console
/// attachment, and sandbox blocks.
pub(crate) type ExpectedRestoreContract<'a> = (
    &'a str,
    &'a str,
    Option<(&'a MicrovmNetworkConfig, &'a SnapshotAttachment)>,
    Option<(
        &'a MicrovmFilesystemConfig,
        &'a Path,
        &'a SnapshotAttachment,
    )>,
    Option<&'a SnapshotAttachment>,
    Vec<SnapshotMicrovmSandboxBlock>,
);

/// Clock and CPU state recorded at the capture boundary of a restored
/// microVM: host downtime, TSC and APIC frequencies, and the CPU contract.
pub(crate) type RestoreTime = (Duration, u64, Option<u64>, Vec<u8>);

/// Validates the authoritative machine contract of a microVM snapshot against
/// the restore-time configuration.
pub(crate) fn validate_restore_contract(
    manifest: &SnapshotManifest,
    expected_memory_size: u64,
    expected_vp_count: u32,
    (
        expected_hypervisor,
        effective_command_line,
        network,
        filesystem,
        console_attachment,
        sandbox_blocks,
    ): ExpectedRestoreContract<'_>,
) -> anyhow::Result<RestoreTime> {
    let saved_contract = manifest
        .machine_contract
        .as_ref()
        .context("microVM snapshot is missing its authoritative machine contract")?;
    let filesystem_slot = microvm_filesystem_slot_from_snapshot(saved_contract)?;
    let filesystem = saved_contract
        .microvm_filesystem
        .as_ref()
        .and(filesystem)
        .map(|(config, root_path, attachment)| (config, root_path, attachment.clone()));
    let expected_contract = openvmm_helpers::snapshot::microvm::microvm_machine_contract(
        expected_hypervisor,
        openvmm_helpers::snapshot::microvm::MICROVM_BOOT_LAYOUT_VERSION,
        effective_command_line.to_owned(),
        network.map(|(config, attachment)| (config, attachment.clone())),
        filesystem_slot,
        filesystem,
        console_attachment.cloned(),
        sandbox_blocks,
        expected_vp_count,
        expected_memory_size,
        saved_contract.state_unit_names.clone(),
        saved_contract.capture_wall_clock,
        saved_contract.tsc_frequency_hz,
        saved_contract.apic_frequency_hz,
        saved_contract.cpu_contract.clone(),
    )?;
    openvmm_helpers::snapshot::microvm::validate_microvm_machine_contract(
        manifest,
        &expected_contract,
    )?;
    let capture_time: std::time::SystemTime = saved_contract
        .capture_wall_clock
        .try_into()
        .context("snapshot capture wall clock is invalid")?;
    let downtime = calculate_snapshot_downtime(capture_time, std::time::SystemTime::now())?;
    Ok((
        downtime,
        saved_contract.tsc_frequency_hz,
        saved_contract.apic_frequency_hz,
        saved_contract.cpu_contract.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn snapshot_downtime_accepts_supported_elapsed_time() {
        let capture = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1);

        assert_eq!(
            calculate_snapshot_downtime(capture, capture).unwrap(),
            Duration::ZERO
        );
        assert_eq!(
            calculate_snapshot_downtime(capture, capture + MAX_SNAPSHOT_DOWNTIME).unwrap(),
            MAX_SNAPSHOT_DOWNTIME
        );
    }

    #[test]
    fn snapshot_downtime_rejects_rollback_and_excessive_elapsed_time() {
        let capture = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1);

        let rollback =
            calculate_snapshot_downtime(capture, std::time::SystemTime::UNIX_EPOCH).unwrap_err();
        assert!(
            rollback
                .to_string()
                .contains("before snapshot capture time")
        );

        let excessive = calculate_snapshot_downtime(
            capture,
            capture + MAX_SNAPSHOT_DOWNTIME + Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(excessive.to_string().contains("exceeds the supported"));
    }
}
