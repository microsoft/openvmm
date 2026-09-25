// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM snapshot restore-time contract checks.

use crate::Options;
use crate::cli_args::microvm::MachineProfileCli;
use anyhow::Context;
use openvmm_defs::microvm::MicrovmNetworkConfig;
use openvmm_helpers::snapshot::SnapshotManifest;
use openvmm_helpers::snapshot::microvm::SnapshotAttachment;
use openvmm_helpers::snapshot::microvm::SnapshotMachineContract;
use openvmm_helpers::snapshot::restore::OpenedSnapshot;
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
}

/// Validates a snapshot restore against the microVM profile and prepares the
/// restore-time inputs of the microVM configuration.
///
/// This may adjust `opt`: the snapshot selects the RAM size.
pub(crate) fn prepare_restore(
    opt: &mut Options,
    restore_snapshot: Option<&OpenedSnapshot>,
) -> anyhow::Result<MicrovmRestore> {
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
        Some(contract.clone())
    } else {
        None
    };
    Ok(MicrovmRestore {
        machine_contract: restore_machine_contract,
    })
}

/// The machine contract a microVM snapshot must match to be restored: the
/// hypervisor, effective command line, network, and boot console attachment.
pub(crate) type ExpectedRestoreContract<'a> = (
    &'a str,
    &'a str,
    Option<(&'a MicrovmNetworkConfig, &'a SnapshotAttachment)>,
    Option<&'a SnapshotAttachment>,
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
        console_attachment,
    ): ExpectedRestoreContract<'_>,
) -> anyhow::Result<RestoreTime> {
    let saved_contract = manifest
        .machine_contract
        .as_ref()
        .context("microVM snapshot is missing its authoritative machine contract")?;
    let expected_contract = openvmm_helpers::snapshot::microvm::microvm_machine_contract(
        expected_hypervisor,
        openvmm_helpers::snapshot::microvm::MICROVM_BOOT_LAYOUT_VERSION,
        effective_command_line.to_owned(),
        network.map(|(config, attachment)| (config, attachment.clone())),
        console_attachment.cloned(),
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
