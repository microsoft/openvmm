// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot restore support for the VM worker: the restore inputs taken from
//! the worker parameters, the restore state kept by [`LoadedVm`], the steps
//! around applying the saved state, and the post-restore start sequence
//! (readiness event and VP release).

use super::LoadedVm;
use super::clock;
use super::clock::RestoreTime;
use crate::partition::HvlitePartition;
use anyhow::Context;
use membacking::FileMappingMode;
use membacking::SharedMemoryBacking;
use openvmm_defs::profile::ProfileSpan;
use openvmm_defs::worker::RESTORE_READY_EVENT_V1;
use openvmm_defs::worker::SavedState;
use openvmm_defs::worker::SharedMemoryFd;
use openvmm_defs::worker::SnapshotRestoreGuards;
use openvmm_defs::worker::VmWorkerParameters;
use state_unit::StateUnits;
use std::fs::File;
use std::io::Write as _;
use vmm_core::partition_unit::StopGuard;

/// Snapshot-restore inputs taken from the [`VmWorkerParameters`].
pub(super) struct RestoreParameters {
    /// Restore state handed to the loaded VM.
    pub(super) state: SnapshotRestore,
    /// Saved canonical CPU contract required by restore.
    pub(super) cpu_contract: Option<Vec<u8>>,
    /// Mapping mode of the file-backed guest RAM handle.
    file_mapping_mode: FileMappingMode,
    /// Snapshot generation handles that must outlive the restored VM.
    pub(super) guards: Option<SnapshotRestoreGuards>,
}

impl RestoreParameters {
    /// Takes the restore inputs out of the worker parameters and validates
    /// the saved time contract.
    pub(super) fn take(parameters: &mut VmWorkerParameters) -> anyhow::Result<Self> {
        let guards = parameters.snapshot_restore_guards.take();
        let ready_sink = parameters.restore_ready_sink.take();
        let restore_time = clock::restore_time_contract(
            parameters.restore_downtime,
            parameters.restore_tsc_frequency_hz,
            parameters.restore_apic_frequency_hz,
        )?;
        tracing::debug!(?restore_time, "received snapshot restore time contract");
        let cpu_contract = parameters.restore_cpu_contract.take();
        let file_mapping_mode = if parameters.shared_memory_copy_on_write {
            FileMappingMode::CopyOnWrite
        } else {
            FileMappingMode::Shared
        };
        Ok(Self {
            state: SnapshotRestore {
                time: restore_time,
                ready_sink,
                ..Default::default()
            },
            cpu_contract,
            file_mapping_mode,
            guards,
        })
    }

    /// Wraps the file-backed guest RAM handle with the restore's mapping mode.
    pub(super) fn shared_memory_backing(&self, fd: SharedMemoryFd) -> SharedMemoryBacking {
        SharedMemoryBacking::from_mappable_with_mode(fd.into(), self.file_mapping_mode)
    }
}

/// Snapshot-restore state of a [`LoadedVm`].
#[derive(Default)]
pub(super) struct SnapshotRestore {
    /// Saved guest-clock contract, checked and applied around the restore.
    time: Option<RestoreTime>,
    /// Whether the VM was loaded from saved state.
    restored_from_snapshot: bool,
    /// Holds the VPs stopped after a restore until the VM first starts.
    start_guard: Option<StopGuard>,
    /// Single-use sink for the restore readiness event.
    ready_sink: Option<File>,
}

#[cfg(guest_arch = "x86_64")]
pub(super) fn validate_restore_cpu_contract(
    partition: &dyn HvlitePartition,
    expected_cpu_contract: Option<Vec<u8>>,
) -> anyhow::Result<()> {
    if let Some(expected_cpu_contract) = expected_cpu_contract {
        let destination_contract = partition.cpu_compatibility_contract();
        let destination_cpu_contract = mesh::payload::encode(destination_contract.clone());
        if destination_cpu_contract != expected_cpu_contract {
            let expected_contract: virt::x86::CpuCompatibilityContract =
                mesh::payload::decode(&expected_cpu_contract)
                    .context("failed to decode snapshot CPU contract")?;
            let first_cpuid_difference = expected_contract
                .cpuid
                .iter()
                .zip(&destination_contract.cpuid)
                .find(|(expected, destination)| expected != destination);
            anyhow::bail!(
                "destination CPU contract does not match the snapshot; first CPUID difference: {first_cpuid_difference:?}"
            );
        }
    }
    Ok(())
}

#[cfg(not(guest_arch = "x86_64"))]
pub(super) fn validate_restore_cpu_contract(
    _partition: &dyn HvlitePartition,
    expected_cpu_contract: Option<Vec<u8>>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        expected_cpu_contract.is_none(),
        "snapshot CPU contracts are only supported for x86-64 guests"
    );
    Ok(())
}

/// Validates the state-unit inventory of a saved-state envelope. Saved state
/// without an inventory is restored without the check.
pub(super) fn validate_inventory(
    state_units: &StateUnits,
    state: &SavedState,
) -> anyhow::Result<()> {
    if !state.inventory.is_empty() {
        state_units.validate_inventory(&state.inventory)?;
    }
    Ok(())
}

impl LoadedVm {
    /// Prepares to restore `saved_state` into the loaded VM: checks the saved
    /// time contract against the saved state and the destination clocks, and
    /// starts the profile span of the restore.
    pub(super) fn begin_snapshot_restore(
        &mut self,
        #[cfg_attr(not(guest_arch = "x86_64"), expect(unused_variables))] saved_state: &SavedState,
    ) -> anyhow::Result<ProfileSpan> {
        self.snapshot_restore.restored_from_snapshot = true;
        let restore_time = self.snapshot_restore.time;

        #[cfg(guest_arch = "x86_64")]
        clock::validate_snapshot_restore_partition_presence(saved_state, restore_time)?;

        self.validate_restore_clock(restore_time)?;
        Ok(ProfileSpan::start())
    }

    /// Completes a restore after the saved state is applied: advances the
    /// guest clocks by the snapshot downtime and holds the VPs stopped until
    /// the VM first starts.
    pub(super) async fn finish_snapshot_restore(
        &mut self,
        saved_state_restore: ProfileSpan,
    ) -> anyhow::Result<()> {
        saved_state_restore.complete("restore", "saved_state_restore", Default::default());
        self.advance_restored_clock(self.snapshot_restore.time)
            .await?;
        self.snapshot_restore.start_guard =
            Some(self.inner.partition_unit.temporarily_stop_vps().await);
        Ok(())
    }

    /// Starts the state units for [`LoadedVm::resume`], sequencing the
    /// restore readiness event and the release of the restored VPs around the
    /// start.
    pub(super) async fn start_state_units(&mut self) -> anyhow::Result<()> {
        let device_start = ProfileSpan::start();
        self.state_units
            .start()
            .await
            .context("VM state units failed to start")?;
        if self.snapshot_restore.restored_from_snapshot && openvmm_defs::profile::enabled() {
            let faults = self.inner.memory_manager.fault_counters();
            device_start.complete(
                "restore",
                "device_start",
                openvmm_defs::profile::ProfileCounters {
                    gpa_faults: Some(faults.guest_faults),
                    populated_bytes: Some(faults.populated_bytes),
                    ..Default::default()
                },
            );
        }
        if let Some(mut sink) = self.snapshot_restore.ready_sink.take() {
            let signal_result = sink.write_all(RESTORE_READY_EVENT_V1).and_then(|()| {
                #[cfg(windows)]
                {
                    sink.sync_all()
                }
                #[cfg(not(windows))]
                {
                    sink.flush()
                }
            });
            if let Err(error) = signal_result {
                self.state_units.stop().await;
                return Err(error).context("failed to publish restore readiness event");
            }
        }
        self.snapshot_restore.start_guard.take();
        Ok(())
    }
}
