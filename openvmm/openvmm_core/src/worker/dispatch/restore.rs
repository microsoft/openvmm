// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot restore support for the VM worker: the restore inputs taken from
//! the worker parameters, the restore state kept by [`LoadedVm`], the steps
//! around applying the saved state, and the post-restore start sequence
//! (readiness event and VP release).

use super::LoadedVm;
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
    /// Mapping mode of the file-backed guest RAM handle.
    file_mapping_mode: FileMappingMode,
    /// Snapshot generation handles that must outlive the restored VM.
    pub(super) guards: Option<SnapshotRestoreGuards>,
}

impl RestoreParameters {
    /// Takes the restore inputs out of the worker parameters.
    pub(super) fn take(parameters: &mut VmWorkerParameters) -> anyhow::Result<Self> {
        let guards = parameters.snapshot_restore_guards.take();
        let ready_sink = parameters.restore_ready_sink.take();
        let file_mapping_mode = if parameters.shared_memory_copy_on_write {
            FileMappingMode::CopyOnWrite
        } else {
            FileMappingMode::Shared
        };
        Ok(Self {
            state: SnapshotRestore {
                ready_sink,
                ..Default::default()
            },
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
    /// Whether the VM was loaded from saved state.
    restored_from_snapshot: bool,
    /// Holds the VPs stopped after a restore until the VM first starts.
    start_guard: Option<StopGuard>,
    /// Single-use sink for the restore readiness event.
    ready_sink: Option<File>,
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
    /// Prepares to restore saved state into the loaded VM: records that the
    /// VM is restored, and starts the profile span of the restore.
    pub(super) fn begin_snapshot_restore(&mut self) -> ProfileSpan {
        self.snapshot_restore.restored_from_snapshot = true;
        ProfileSpan::start()
    }

    /// Completes a restore after the saved state is applied: holds the VPs
    /// stopped until the VM first starts.
    pub(super) async fn finish_snapshot_restore(&mut self, saved_state_restore: ProfileSpan) {
        saved_state_restore.complete("restore", "saved_state_restore", Default::default());
        self.snapshot_restore.start_guard =
            Some(self.inner.partition_unit.temporarily_stop_vps().await);
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
