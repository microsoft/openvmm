// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot restore support for the VM worker: the restore inputs taken from
//! the worker parameters and the checks applied to the saved state.

use membacking::FileMappingMode;
use membacking::SharedMemoryBacking;
use openvmm_defs::worker::SavedState;
use openvmm_defs::worker::SharedMemoryFd;
use openvmm_defs::worker::SnapshotRestoreGuards;
use openvmm_defs::worker::VmWorkerParameters;
use state_unit::StateUnits;

/// Snapshot-restore inputs taken from the [`VmWorkerParameters`].
pub(super) struct RestoreParameters {
    /// Mapping mode of the file-backed guest RAM handle.
    file_mapping_mode: FileMappingMode,
    /// Snapshot generation handles that must outlive the restored VM.
    pub(super) guards: Option<SnapshotRestoreGuards>,
}

impl RestoreParameters {
    /// Takes the restore inputs out of the worker parameters.
    pub(super) fn take(parameters: &mut VmWorkerParameters) -> anyhow::Result<Self> {
        let guards = parameters.snapshot_restore_guards.take();
        let file_mapping_mode = if parameters.shared_memory_copy_on_write {
            FileMappingMode::CopyOnWrite
        } else {
            FileMappingMode::Shared
        };
        Ok(Self {
            file_mapping_mode,
            guards,
        })
    }

    /// Wraps the file-backed guest RAM handle with the restore's mapping mode.
    pub(super) fn shared_memory_backing(&self, fd: SharedMemoryFd) -> SharedMemoryBacking {
        SharedMemoryBacking::from_mappable_with_mode(fd.into(), self.file_mapping_mode)
    }
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
